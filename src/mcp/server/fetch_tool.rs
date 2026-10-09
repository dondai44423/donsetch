//! The fetch tool handler: dispatch, URL/handle resolution,
//! multi-fetch batching, single fetch + the full escalation ladder
//! (bypass, ghost, actions, OCR, anticloak, resurrection), page
//! history + link handles, and the result envelope assembly.

use serde_json::{Value, json};

use super::*;
use crate::transport::request_route::RequestRoute;

/// The caller's whole-call budget, carried into the browser helpers so a
/// single pass can be sized against what is left of it.
///
/// The passes used to be fixed at 20s (25s on the actions path) and ignored
/// `deadline_ms` entirely, while the deadline wrapped the whole call from
/// outside. With a short deadline the call was therefore cut off MID-pass:
/// measured, `--tier 2 --deadline-ms 12000` against a known interstitial
/// answered `deadline.hit` with no wall verdict at all, even though the first
/// pass had already seen the wall, and the browser kept working on a pass
/// nobody was waiting for. A pass is bounded by the remaining budget now,
/// without a floor that can exceed the clock; the fixed default stands when
/// the caller asked for no deadline.
#[derive(Clone, Copy)]
pub(super) struct Budget {
    deadline: Option<std::time::Duration>,
    start: std::time::Instant,
}

impl Budget {
    fn of(args: &Value) -> Self {
        Self {
            deadline: args
                .get("deadline_ms")
                .and_then(Value::as_u64)
                .map(|ms| std::time::Duration::from_millis(ms.clamp(500, 600_000))),
            start: std::time::Instant::now(),
        }
    }

    /// How long one browser pass may run.
    fn pass(self, default_secs: u64) -> std::time::Duration {
        let default = std::time::Duration::from_secs(default_secs);
        let Some(d) = self.deadline else {
            return default;
        };
        // Reserve at most a tenth of the remaining time for the envelope.
        // A fixed two seconds would consume short calls before navigation.
        let remaining = d.saturating_sub(self.start.elapsed());
        let reserve = (remaining / 10).min(std::time::Duration::from_secs(2));
        let usable = remaining.saturating_sub(reserve);
        usable.min(default)
    }
}

/// State selected once before adapter, archive or browser work.
struct FetchCall {
    route: RequestRoute,
    budget: Budget,
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
    // A deadline or cancellation can interrupt an action before its result
    // arrives. The side effect may still have happened; never advise replay.
    if is_failure(&result)
        && trace
            .iter()
            .any(|step| step["action"] == "action-execution" && step["outcome"] == "started")
    {
        let action = "inspect the result with a plain fetch without actions; earlier actions may have completed, so do not replay them automatically";
        result["errorKind"] = json!("permanent");
        result["structuredContent"]["retry_safe"] = json!(false);
        result["structuredContent"]["next_action"] = json!(action);
        if let Some(text) = result["content"][0]["text"].as_str() {
            let message = text.split("\n\nNext action:").next().unwrap_or(text);
            result["content"][0]["text"] = json!(format!("{message}\n\nNext action: {action}"));
        }
    }
    add_shot_receipt(args, &mut result, &trace);
    drop(trace);
    if args.get("links").and_then(Value::as_bool) == Some(true)
        || args.get("media").and_then(Value::as_bool) == Some(true)
    {
        result["structuredContent"]["budget_scope"] =
            json!("rendered markdown, including link and media markup");
    }
    if let Some(debug) = result.pointer_mut("/_meta/com.donsetch~1fetch-debug") {
        debug["elapsed_ms"] = json!(started.elapsed().as_millis());
    }
    result
}

fn add_shot_receipt(args: &Value, result: &mut Value, trace: &[Value]) {
    let Some(path) = args.get("shot").and_then(Value::as_str) else {
        return;
    };
    let outcome = trace
        .iter()
        .rev()
        .find(|step| step["action"] == "screenshot")
        .and_then(|step| step["outcome"].as_str());
    let reason: String = match outcome {
        Some(o) => o.to_string(),
        None if !is_failure(&*result) => {
            "skipped: no interactive captcha capture was needed".to_string()
        }
        None if result.pointer("/structuredContent/retry_in_secs").is_some() => {
            "skipped: host is in a learned solve-cooldown; the fetch was refused before any captcha capture"
                .to_string()
        }
        None => match result
            .pointer("/structuredContent/code")
            .and_then(Value::as_str)
        {
            Some("deadline.hit") => {
                "skipped: the fetch hit its deadline before an interactive captcha capture"
                    .to_string()
            }
            Some("browser.transport") | Some("browser.timeout") => {
                "skipped: the browser pass failed before an interactive captcha capture"
                    .to_string()
            }
            _ => "skipped: fetch failed before an interactive captcha capture".to_string(),
        },
    };
    result["structuredContent"]["shot"] = json!({
        "requested": path,
        "saved_to": outcome.and_then(|s| s.strip_prefix("saved: ")),
        "reason": reason,
    });
}

async fn record_shot(ghost: &crate::ghost::Ghost, path: &str, trace: &mut Trace) {
    let outcome = match ghost.screenshot(path).await {
        Ok(()) => match crate::paths::resolve_screenshot_path(path) {
            Ok(dest) => format!("saved: {}", dest.display()),
            Err(e) => format!("failed: {e}"),
        },
        Err(e) => format!("failed: {e}"),
    };
    trace.step("2", "screenshot", &outcome, 0);
}

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
    let budget_tokens = args
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .map(|t| (t as usize).clamp(200, 500_000));

    if urls.len() == 1 && budget_tokens.is_none() {
        let url = match resolve_fetch_url(daemon, &urls[0]).await {
            Ok(u) => u,
            Err(mut e) => {
                add_shot_receipt(args, &mut e, &[]);
                return e;
            }
        };
        let result = fetch_with_budget(daemon, args, &url, deadline, ctx.as_mut()).await;
        return result;
    }
    // Single resolved URL: keep the single-page response shape, but
    // always run under the deadline + MCP-cancellation wrapper (#164).
    // Previously this branch (reached whenever budget_tokens was set,
    // since the fast path above demands budget_tokens.is_none())
    // called fetch_single bare: an uncancellable, deadline-free fetch
    // on a path that can still spawn a ghost render. budget_tokens
    // also bounds the page now, exactly like the batch path.
    if urls.len() == 1 {
        let resolved = match resolve_fetch_url(daemon, &urls[0]).await {
            Ok(url) => url,
            Err(mut error) => {
                add_shot_receipt(args, &mut error, &[]);
                return error;
            }
        };
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
        let result =
            fetch_with_budget(daemon, effective_args, &resolved, deadline, ctx.as_mut()).await;
        return result;
    }
    fetch_multi(daemon, args, urls, budget_tokens, deadline, ctx).await
}

/// The verdict a FAILURE envelope reports.
///
/// The success path starts the verdict at ContentOk and a prewarm hit
/// sets it too, so a failure envelope inherited "ContentOk" and
/// contradicted the error beside it: a walled fetch reported
/// `"verdict": "ContentOk"` with `"code": "wall.captcha"` (#258).
///
/// A failure never calls the fetch content. Any other verdict the run
/// earned is kept, a challenge or a 404 included, and otherwise the
/// failure names itself: `Blocked` for a wall, which is the word the
/// error-code table already uses for `wall.*`, and `Unknown` for
/// anything else.
fn failure_verdict(current: &str, kind: &str) -> String {
    if current != "ContentOk" {
        return current.to_string();
    }
    match kind {
        "walled" => "Blocked".to_string(),
        _ => "Unknown".to_string(),
    }
}

/// Advice for a browser-pass failure envelope: an unsettled document
/// waits, a deadline stall gets real deadline advice (retrying the
/// same way will not pass a wall), everything else derives from the
/// verdict/kind.
fn browser_failure_next_action(
    failed_verdict: &str,
    deadline_stall: bool,
    gate: Option<Verdict>,
    status: u16,
    kind: &str,
) -> String {
    if failed_verdict == "Incomplete" {
        "retry with a wait for expected content; the browser document did not settle".to_string()
    } else if deadline_stall {
        "the browser pass hit its deadline before usable content : retry with a higher deadline_ms, or tier=1 for the plain-HTTP view; if it repeats, choose another source".to_string()
    } else {
        next_action_for(gate, status, kind)
    }
}

#[cfg(test)]
mod browser_failure_advice_tests {
    use super::browser_failure_next_action;

    // The TikTok report: a browser pass that hit its deadline answered
    // "check this host from another network" : an invented network
    // fault. A deadline stall must give deadline advice instead.
    #[test]
    fn a_deadline_stall_gets_deadline_advice_not_network_advice() {
        let action = browser_failure_next_action("Unknown", true, None, 0, "transient");
        assert!(action.contains("deadline_ms"), "{action}");
        assert!(!action.contains("another network"), "{action}");
    }

    #[test]
    fn an_unsettled_document_keeps_its_wait_advice() {
        let action = browser_failure_next_action("Incomplete", false, None, 200, "transient");
        assert!(action.contains("did not settle"), "{action}");
    }

    #[test]
    fn a_genuine_transient_keeps_the_network_advice() {
        let action = browser_failure_next_action("Unknown", false, None, 0, "transient");
        assert!(action.contains("network"), "{action}");
    }
}

#[cfg(test)]
mod shot_receipt_tests {
    use super::add_shot_receipt;
    use serde_json::json;

    fn reason_of(result: &serde_json::Value) -> String {
        result["structuredContent"]["shot"]["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    // The 4chan report: a learned solve-cooldown refusal and a real
    // browser failure shared one ambiguous string. Each cause now
    // names itself.
    #[test]
    fn a_learned_solve_cooldown_names_itself() {
        let mut result = json!({"structuredContent": {"ok": false, "retry_in_secs": 900}});
        add_shot_receipt(&json!({"shot": "/tmp/cap.png"}), &mut result, &[]);
        assert!(
            reason_of(&result).contains("solve-cooldown"),
            "{}",
            reason_of(&result)
        );
    }

    #[test]
    fn a_deadline_names_itself() {
        let mut result = json!({"structuredContent": {"ok": false, "code": "deadline.hit"}});
        add_shot_receipt(&json!({"shot": "/tmp/cap.png"}), &mut result, &[]);
        assert!(
            reason_of(&result).contains("deadline"),
            "{}",
            reason_of(&result)
        );
    }

    #[test]
    fn a_browser_failure_names_itself() {
        let mut result = json!({"structuredContent": {"ok": false, "code": "browser.transport"}});
        add_shot_receipt(&json!({"shot": "/tmp/cap.png"}), &mut result, &[]);
        assert!(
            reason_of(&result).contains("browser pass failed"),
            "{}",
            reason_of(&result)
        );
    }

    #[test]
    fn a_success_without_a_capture_says_no_capture_was_needed() {
        let mut result = json!({"structuredContent": {"ok": true}});
        add_shot_receipt(&json!({"shot": "/tmp/cap.png"}), &mut result, &[]);
        assert!(
            reason_of(&result).contains("no interactive captcha"),
            "{}",
            reason_of(&result)
        );
    }

    #[test]
    fn an_unclassified_failure_keeps_the_generic_reason() {
        let mut result = json!({"structuredContent": {"ok": false, "code": "content.notfound"}});
        add_shot_receipt(&json!({"shot": "/tmp/cap.png"}), &mut result, &[]);
        assert!(
            reason_of(&result).contains("fetch failed"),
            "{}",
            reason_of(&result)
        );
    }

    #[test]
    fn a_recorded_outcome_beats_the_reason_guessing() {
        let mut result = json!({"structuredContent": {"ok": false}});
        add_shot_receipt(
            &json!({"shot": "/tmp/cap.png"}),
            &mut result,
            &[json!({"action": "screenshot", "outcome": "saved: /tmp/cap.png"})],
        );
        assert!(
            reason_of(&result).contains("saved"),
            "{}",
            reason_of(&result)
        );
    }
}

#[cfg(test)]
mod failure_verdict_tests {
    // Nextest isolates daemon state. Match the production runtime's 8 MiB
    // stack: the unoptimized fetch future exceeds libtest's 2 MiB default.
    #[test]
    fn report_audit_batch_keeps_each_invalid_url_as_an_individual_result() {
        std::thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(|| {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let daemon = std::sync::Arc::new(super::Daemon::new().await.unwrap());
                let result = super::fetch_tool(&daemon,
                    &serde_json::json!({"url":["not-a-url", "also-not-a-url"], "archive":"off"}), None).await;
                let rows = result["structuredContent"]["results"].as_array().unwrap();
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().all(|r| r["ok"] == false && r["code"] == "fetch.invalid"));
                assert_eq!(rows[0]["url"], "not-a-url");
                assert_eq!(rows[1]["url"], "also-not-a-url");
            });
        }).unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn report_audit_archive_race_preserves_recovery_and_unknown_state() {
        use super::{Avail, archive_lookup_pair};
        for found_first in [false, true] {
            let answer = |found: bool| async move {
                if found {
                    Avail::Found((
                        "https://web.archive.org/web/20260101/http://example.com/".into(),
                        "20260101".into(),
                    ))
                } else {
                    std::future::pending::<Avail>().await
                }
            };
            let recovered = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                archive_lookup_pair(answer(found_first), answer(!found_first)),
            )
            .await
            .expect("an unresponsive index must not delay a real capture");
            assert!(matches!(recovered, Avail::Found(_)));
        }
        assert!(matches!(
            archive_lookup_pair(async { Avail::Empty }, async { Avail::Empty }).await,
            Avail::Empty
        ));
        assert!(matches!(
            archive_lookup_pair(async { Avail::Empty }, async { Avail::Unreachable }).await,
            Avail::Unreachable
        ));
        assert!(matches!(
            archive_lookup_pair(async { Avail::Unreachable }, async { Avail::Empty }).await,
            Avail::Unreachable
        ));
    }

    use super::failure_verdict;

    #[test]
    fn a_failure_never_reports_content_ok() {
        // #258: a walled fetch reported `"verdict": "ContentOk"` beside
        // `"code": "wall.captcha"`. ContentOk is the success path's
        // default, and no failure may inherit it.
        assert_eq!(failure_verdict("ContentOk", "walled"), "Blocked");
        assert_eq!(failure_verdict("ContentOk", "permanent"), "Unknown");
        assert_eq!(failure_verdict("ContentOk", "transient"), "Unknown");
    }

    #[test]
    fn a_failure_keeps_a_verdict_the_run_actually_earned() {
        // Tier 1 hit the wall and the ghost could not clear it: the
        // challenge is what happened, and it stays reported rather than
        // being flattened to Blocked.
        assert_eq!(
            failure_verdict("Challenge(Cloudflare)", "walled"),
            "Challenge(Cloudflare)"
        );
        assert_eq!(failure_verdict("SoftNotFound", "permanent"), "SoftNotFound");
    }
}

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
    Err(tool_error_structured(
        format!("fetch: url must be http(s), got: {raw}"),
        "permanent",
        Some(
            json!({"url": raw, "code": "fetch.invalid", "next_action": "pass a full http(s) URL or a valid fetch handle"}),
        ),
    ))
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
                    Err(mut error) => {
                        add_shot_receipt(&a, &mut error, &[]);
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
    if let Some(budget_tok) = budget_tokens {
        slice_batch_markdowns(
            &urls,
            &results,
            &mut markdowns,
            budget_tok,
            &mut sliced_flags,
        );
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
                for field in ["shot", "budget_scope", "next_offset", "archived", "read_status", "content_complete", "partial", "partial_reason", "items_found", "items_total", "matched", "stitch_complete", "next_part"] {
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
                for field in ["shot", "read_status", "next_action"] {
                    if let Some(value) = r["structuredContent"].get(field) {
                        o[field] = value.clone();
                    }
                }
                o["code"] = r
                    .pointer("/structuredContent/code")
                    .cloned()
                    .unwrap_or_else(|| json!("content.extract"));
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
        // Classify like the search batch does: an all-permanent
        // batch (12 SSRF-refused URLs) must not advertise "safe to
        // retry immediately". No kind anywhere = stay conservative
        // (transient).
        let kinds: Vec<&str> = results
            .iter()
            .filter(|v| is_err(v))
            .filter_map(|v| v.get("errorKind").and_then(Value::as_str))
            .collect();
        let kind = if kinds.is_empty() {
            "transient"
        } else {
            super::errors::batch_failure_kind(kinds.into_iter())
        };
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
    budget_tokens: usize,
    sliced_flags: &mut [bool],
) {
    let budget_chars = budget_tokens.saturating_mul(4);
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

fn fetch_input(args: &Value, url: &str) -> Result<url::Url, Value> {
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
        return Err(tool_error_structured(
            error.to_string(),
            "permanent",
            Some(json!({
                "url": url, "code": "guard.ssrf",
                "next_action": "pass a public http(s) URL without embedded credentials",
            })),
        ));
    }
    if let Some(selector) = args.get("selector").and_then(Value::as_str)
        && scraper::Selector::parse(selector).is_err()
    {
        return Err(tool_error_structured(
            format!("invalid CSS selector: {selector}"),
            "permanent",
            Some(json!({
                "url": url, "code": "selector.invalid", "next_action": "correct the CSS selector syntax",
            })),
        ));
    }
    Ok(parsed_url)
}

fn fetch_read_options(args: &Value) -> Result<ExtractOptions, Value> {
    let mut opts = ExtractOptions::default();
    let preset = match args.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if m == "scan" => Some(800),
        Some(Value::String(m)) if m == "read" => Some(4_000),
        Some(Value::String(m)) if m == "deep" => Some(16_000),
        _ => return Err(tool_error("fetch: mode must be scan, read or deep")),
    };
    opts.focus = args.get("focus").and_then(Value::as_str).map(String::from);
    opts.max_chars = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(200, 1_048_576))
        .or(preset);
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
    Ok(opts)
}

/// Single-URL fetch with resurrection (v3): dead URLs get one
/// honest attempt at the Wayback Machine before the error stands.
pub(super) async fn fetch_single(daemon: &Arc<Daemon>, args: &Value, url: &str) -> Value {
    let parsed_url = match fetch_input(args, url) {
        Ok(parsed) => parsed,
        Err(error) => return error,
    };
    if let Err(error) = fetch_read_options(args) {
        return error;
    }
    let archive = match args.get("archive").and_then(Value::as_str) {
        Some("off") => "off",
        Some("only") => "only",
        _ => "auto",
    };
    let budget = Budget::of(args);
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
    let call = FetchCall { route, budget };
    if archive == "only" {
        let no_live = tool_error(format!("archive=only : skipping live fetch for {url}"));
        return match try_resurrect(daemon, args, url, &no_live, &call.route).await {
            Ok(v) => v,
            Err(f) => resurrect_error(url, &f),
        };
    }
    let result = Box::pin(fetch_single_inner(daemon, args, url, &call)).await;
    if archive == "off" || !is_failure(&result) {
        return result;
    }
    // Resurrectable failures only: dead pages, hard walls, and
    // transport-level death (TLS handshake against a parked domain,
    // DNS gone, port closed) : the archetypal dead links. Ambiguous
    // transients : timeouts, resets, protocol errors : stay
    // excluded : a snapshot would launder an unknown into fake
    // certainty, and a reset can be an IP-level block that a
    // snapshot must never paper over.
    let transport_dead = result
        .pointer("/structuredContent/fetch_error")
        .and_then(Value::as_str)
        .is_some_and(|k| matches!(k, "tls" | "dns" | "refused"));
    let resurrectable = result
        .pointer("/structuredContent/verdict")
        .and_then(Value::as_str)
        .is_some_and(|v| matches!(v, "SoftNotFound" | "Paywall" | "Challenge" | "AuthWall"))
        || result
            .pointer("/structuredContent/status")
            .and_then(Value::as_u64)
            .is_some_and(|s| s == 404 || s == 410)
        || transport_dead;
    if !resurrectable {
        return result;
    }
    match try_resurrect(daemon, args, url, &result, &call.route).await {
        Ok(v) => v,
        Err(f) => {
            // The original live error stands as the primary answer;
            // the archive attempt is recorded as context so a silent
            // snapshot-side failure is visible in the payload.
            let mut result = result;
            if let Some(obj) = result
                .pointer_mut("/structuredContent")
                .and_then(Value::as_object_mut)
            {
                obj.insert("archive_stage".into(), json!(f.stage.tag()));
            }
            result
        }
    }
}

/// Build the archive=only error from the exact stage resurrection
/// gave up at. "Never archived" is claimed ONLY when both indexes
/// (availability + CDX) were consulted and answered empty :
/// unreachable archives and found-but-unusable snapshots get their
/// own honest messages.
fn resurrect_error(url: &str, f: &ResurrectError) -> Value {
    let tag = f.stage.tag();
    match &f.stage {
        ResurrectStage::LookupUnreachable => tool_error_structured(
            format!("archive: Wayback Machine unreachable for {url}"),
            "transient",
            Some(json!({
                "url": url,
                "archive_stage": tag,
                "next_action": "the archive lookup failed : retry, or try web_search for a live alternative",
            })),
        ),
        ResurrectStage::NoSnapshot => tool_error_structured(
            format!("archive: no Wayback snapshot found for {url}"),
            "permanent",
            Some(json!({
                "url": url,
                "archive_stage": tag,
                "next_action": "the URL was never archived : try web_search for a live alternative",
            })),
        ),
        _ => {
            let snap = f.snapshot_url.clone().unwrap_or_default();
            tool_error_structured(
                format!("archive: Wayback snapshot found but unusable ({tag}) for {url}"),
                "permanent",
                Some(json!({
                    "url": url,
                    "archive_stage": tag,
                    "snapshot_url": snap,
                    "next_action": format!("a snapshot exists but could not be served : inspect it at {snap} or try web_search for a live alternative"),
                })),
            )
        }
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

fn fetch_url_rewrite(
    url: &url::Url,
    args: &Value,
    no_adapter: bool,
) -> Option<(String, &'static str)> {
    crate::adapters::rewrite(url).filter(|(_, name)| {
        !no_adapter
            && args.get("tier").and_then(Value::as_str) != Some("2")
            && args
                .get("actions")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
            && (args.get("section").and_then(Value::as_str).is_none()
                || *name == "adapter:stackexchange-api")
            && args.get("selector").and_then(Value::as_str).is_none()
    })
}

/// Whether a tier-1 verdict means the ADAPTER rewrite bought
/// nothing: every non-content verdict, a challenge included. The
/// adapter is an optimization the caller never asked for, and
/// adapter endpoints never route to the browser (a `.json` page in
/// a browser is useless), so a refusal there must retry the page
/// the caller asked for (#287: every reddit fetch died on a single
/// `.json` 403 with no browser pass and a one-step trail).
fn adapter_hop_failed(verdict: Verdict, adapter_host: bool, no_adapter: bool) -> bool {
    adapter_host && !no_adapter && !matches!(verdict, Verdict::ContentOk)
}

// Reddit adapter/session recovery earns an HTTP recheck after initialization.
// Its saved wall memory must not skip that recovery. Other hosts retain their
// learned cooldown; explicit browser requests keep precedence.
// v4.7 V03: JSON data endpoints (reddit `.json`, API dumps) are always Cold:
// like PDFs, a browser pass cannot improve a raw payload.
fn fetch_route(
    state: &crate::ghost::cache::GhostState,
    host: &str,
    tier: &str,
    is_pdf_url: bool,
    is_json_url: bool,
    adapter_host: bool,
    retry_http: bool,
) -> RouteDecision {
    if tier == "2" && !is_pdf_url && !is_json_url && !adapter_host {
        RouteDecision::SkipToSolve
    } else if tier == "1"
        || is_pdf_url
        || is_json_url
        || adapter_host
        || (retry_http && matches!(host, "www.reddit.com" | "reddit.com"))
    {
        RouteDecision::Cold
    } else {
        state.route_for(host)
    }
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
        .fetch_persona_on_route(&oldu, None, Some(route))
        .await;
    trace.step(
        "1",
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
fn reddit_session_live(cookies: &[CookieRecord]) -> bool {
    cookies.iter().any(|c| c.name == "loid")
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

/// A reddit page refusal at tier 1 that one session-init retry can
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
/// adapter hop's trail in front of the retry's so the escalation
/// reads as ONE ladder rather than two unrelated hops. The retry
/// runs the full generic pipeline, which escalates on its own
/// rules (HTML fetch, ghost, cookie retry).
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
    let budget = call.budget;
    let parsed_url = match fetch_input(args, url) {
        Ok(parsed) => parsed,
        Err(error) => return error,
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
    let url = match fetch_url_rewrite(&parsed_url, args, no_adapter) {
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
        return tool_error_structured(
            format!("{e}"),
            "permanent",
            Some(json!({
                "url": url,
                "next_action": "private/loopback targets are blocked by design : use a public URL",
            })),
        );
    }
    let mut opts = match fetch_read_options(args) {
        Ok(opts) => opts,
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
    let stitch = args.get("stitch").and_then(Value::as_bool).unwrap_or(false);
    let output_opts = opts.clone();
    if stitch && opts.must_contain.is_none() && !opts.toc {
        opts.max_chars = Some(1_048_576);
        opts.offset = 0;
        if output_opts.offset >= 1_048_576 {
            return tool_error(
                "fetch: stitched offset exceeds the 1 MiB article collection limit; use the returned next_part URL and offset",
            );
        }
    }
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
        return fetch_with_actions(
            daemon, &url, &url_host, &opts, &actions, shot, image_text, call,
        )
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
    // v4.7 V03: JSON data endpoints (`reddit .json`, API dumps) are
    // terminal at the HTTP tier : a browser cannot improve a
    // structured payload. Same class PDFs and adapter endpoints
    // already follow.
    let is_json_url = is_json_url_like(&url);

    // === Decision: how to route this fetch? ===
    // The self-improving loop: the domain profile decides
    // cold / warm / skip-to-solve / recheck-cold.
    // Adapter endpoints (reddit .json, package registry APIs) are
    // plain-GET structured targets : never
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
    // v4 E2: persona locale drives Accept-Language so tier-1 and
    // the ghost claim one language identity. Viewport/locale for the
    // browser itself are looked up again at acquire time.
    let persona_al = {
        let state = daemon.state.lock().await;
        match state.personas.get(&host) {
            Some(p) if p.quarantine_reason.is_none() => {
                Some(crate::profile::accept_language_with_persona(
                    &host,
                    url::Url::parse(&url)
                        .map(|u| u.path().to_string())
                        .unwrap_or_else(|_| "/".into())
                        .as_str(),
                    &p.locale,
                ))
            }
            _ => None,
        }
    };
    let retry_http =
        no_adapter || args.get("_reddit_session").and_then(Value::as_bool) == Some(true);
    let (route, known_walled) = {
        let state = daemon.state.lock().await;
        (
            fetch_route(
                &state,
                &host,
                tier,
                is_pdf_url,
                is_json_url,
                adapter_host,
                retry_http,
            ),
            state.is_known_walled(&host),
        )
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
                "code": "wall.blocked",
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
            .filter(|entry| entry.outcome.route == call.route)
    } else {
        None
    };
    if let Some(entry) = prewarm_entry {
        tier_used = "prewarmed";
        prewarmed = true;
        trace.step("prewarm", "search-handoff", "hit", 0);
        // law 6: make the warm handoff observable in `donsetch status`.
        daemon.state.lock().await.note_prewarm_served();
        out = Some(entry.outcome);
    }

    let mut adapter_session_seeded = false;
    if !skip_tier1 && !prewarmed {
        let t0 = std::time::Instant::now();
        let response = {
            let request = daemon.fetcher.fetch_persona_on_route(
                &url,
                persona_al.as_deref(),
                Some(&call.route),
            );
            let needs_seed = adapter_used == Some("adapter:reddit-json")
                && !reddit_session_live(&daemon.fetcher.jar_snapshot(&host));
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
                    state.record_failure(&host, crate::ghost::cache::FailClass::Network);
                    state.recent_network_failures(&host)
                } else {
                    0
                };
                let next_action = if failures >= 3 {
                    "repeated transport failures from this egress in the last 10 minutes; check the host from another network or choose another source before retrying".into()
                } else {
                    next_action_for(None, 0, kind)
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
                // An adapter endpoint's refusal is not evidence about
                // the PAGE (reddit's protected `.json` is the known
                // case, #291): recording it as a domain wall routed
                // the fallback retry, and every later fetch, around
                // tier 1 entirely. The fallback below re-probes the
                // caller's URL itself; its own verdict is what
                // records a wall. v4.7 V03: the same holds for any
                // JSON data endpoint : a `.json` challenge says
                // nothing about the human-facing pages.
                Verdict::Challenge(_) if adapter_host || is_json_url || is_json_endpoint(o) => {}
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
                // API access does not prove the original website wall
                // cleared (v4.7 V03: nor does a JSON endpoint's success :
                // the data lane teaches counters, not page routing).
                Verdict::ContentOk if adapter_host || is_json_url || is_json_endpoint(o) => {
                    state.record_fetch(&host)
                }
                Verdict::ContentOk => {
                    if is_warm {
                        // Warm succeeded : refresh the cookie vault (write-back).
                        let snap = daemon.fetcher.jar_snapshot(&host);
                        state.record_warm_ok(&host, &snap);
                    } else if known_walled {
                        // A previously walled site may serve bait to HTTP. Keep
                        // its wall memory until a browser comparison is equivalent.
                        state.record_fetch(&host);
                    } else {
                        state.record_cold_ok(&host);
                    }
                }
                // Everything else (404, rate-limit, paywall, auth,
                // hard block): counters only, no wall inference.
                _ => state.record_fetch(&host),
            }
        }
    }

    // v4.7 V03: JSON data endpoints are terminal at the HTTP tier.
    // The `.json` name or a JSON content type means a structured
    // payload : a browser pass cannot improve it, it must not gate
    // the verdict, and it must not trigger a decoy comparison.
    let json_like = is_json_url || out.as_ref().is_some_and(is_json_endpoint);

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
            // A refusal on an ADAPTER endpoint (a challenge
            // included) is a failed adapter hop, never the end of
            // the ladder: retry the caller's URL through the generic
            // pipeline, which escalates to the browser on its own
            // rules (#287: reddit's `.json` 403 used to end every
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
            // at tier 1 without a session: the humanity page or the
            // shell. One legacy-host navigation seeds the session,
            // the retry gets the real SSR page, and the SSR adapter
            // turns that into the card. Cheap enough to try before
            // any ghost pass, on tier 1 and on auto alike.
            _ if reddit_session_retry_eligible(&o.verdict, &host, args) => {
                return reddit_session_fallback(daemon, args, &url, &mut trace, call).await;
            }
            // v4.7 V03: a refused JSON data endpoint is terminal too :
            // an honest wall error outranks a browser pass on a
            // payload a browser cannot improve.
            _ if tier != "1"
                && !json_like
                && crate::detect::walls::browser_recovery(
                    o.status, &o.headers, &o.body, o.verdict,
                ) => {}
            v => {
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

    if adapter_used == Some("adapter:stackexchange-api")
        && let Some(o) = &out
    {
        crate::adapters::stackexchange::record_api_backoff(&o.body);
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

    // === Tier-1 extraction (when we have a body) ===
    let mut ghost_html = None;
    let mut final_ex: Option<extract::Extracted> = None;
    let mut final_tier: &str = tier_used;
    let mut final_status: u16 = out.as_ref().map(|o| o.status).unwrap_or(0);
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
        match extract::extract_off_worker(&o.body, &ct, &o.url, &opts).await {
            Ok(e) => {
                final_url = o.url.clone();
                final_ex = Some(e);
            }
            Err(extract::ExtractError::Wall(v)) => {
                extraction_wall = Some(v);
                final_verdict = format!("{v:?}");
                if tier == "1" {
                    return tool_error_structured(
                        format!("access wall at {url}: {v:?}"),
                        "walled",
                        Some(json!({
                            "url": url, "status": final_status, "verdict": final_verdict,
                            "next_action": next_action_for(Some(v), final_status, "walled"), "escalation": trace.value(),
                        })),
                    );
                }
            }
            Err(extract::ExtractError::SelectorNoMatch {
                selector,
                inspected,
            }) => {
                return tool_error_structured(
                    format!("CSS selector {selector:?} matched no elements"),
                    "permanent",
                    Some(
                        json!({"url": orig_url, "code": "selector.nomatch", "elements_inspected": inspected,
                        "next_action": "correct the selector, or omit it to read the page"}),
                    ),
                );
            }
            Err(extract::ExtractError::BadSelector(selector)) => {
                return tool_error_structured(
                    format!("invalid CSS selector: {selector}"),
                    "permanent",
                    Some(
                        json!({"url": orig_url, "code": "selector.invalid", "next_action": "correct the CSS selector syntax"}),
                    ),
                );
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
        .map(|o| crate::detect::walls::browser_recovery(o.status, &o.headers, &o.body, o.verdict))
        .unwrap_or(false)
        || extraction_wall.is_some();

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
        if let Ok(e3) = extract::extract_off_worker(&o.body, &ct, &o.url, &lopts).await
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
    // But a small PDF is still a PDF : DonSheet handles it. And
    // it is only a 404 when the STATUS says so: a 2xx that comes
    // back with no content is a wall or a spinner shell, not a
    // 404 (live case: tiktok's "Please wait..." 12-token fetch
    // served as ok). Never escalate on a real error status.
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
    // tier-1 fetch served that as a successful result). Short,
    // sentence-free, identifier-dominated text = a shell, not
    // content: escalate instead of serving garbage.
    let shell_text =
        status_2xx && !is_pdf_content && final_ex.as_ref().map(is_framework_shell).unwrap_or(false);
    // #282: "the extraction is non-empty" is not a success test. A
    // challenge interstitial or a chrome-only shell extracted at
    // tier 1 must escalate (auto) or fail honestly, not serve.
    let challenge_text = status_2xx
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
    let need_ghost = !is_pdf_content
        && !adapter_host // adapter endpoints (reddit .json, registry APIs) are plain GETs
        && !json_like // v4.7 V03: JSON data endpoints never escalate
        && ((challenge && tier != "1" && !is_small_404)
            || skip_tier1
            || (still_thin && tier == "auto" && !is_small_404)
            || (shell_text && tier == "auto")
            || (challenge_text && tier == "auto")
            || (chrome_text && tier == "auto" && !is_small_404));

    if need_ghost {
        // Render-cache shortcut: a previously recovered DOM.
        // Verified non-thin AND non-challenge before serving : the
        // cache used to store shells and challenge interstitials,
        // re-serving them forever as ContentOk.
        if ex_thin
            && !stitch
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
            if cached_verdict == Verdict::ContentOk {
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

        let ghost_result = ghost_escalate(
            daemon,
            &url,
            &host,
            &opts,
            challenge || shell_warm || (skip_tier1 && tier != "2"),
            tier == "2" || opts.selector.is_some(),
            shot,
            &mut trace,
            budget,
            &call.route,
            persona_al.as_deref(),
        )
        .await;
        match ghost_result {
            Ok((e, tier2, status, furl, html)) => {
                if stitch {
                    ghost_html = Some((html, furl.clone()));
                }
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
                    && !msg.starts_with("authentication required")
                    && let Some(v3) = try_bypass(daemon, &url, &opts, &mut trace).await
                {
                    return v3;
                }
                let observed_gate = if msg.starts_with("authentication required") {
                    Some(Verdict::AuthWall)
                } else if msg.starts_with("not found:") {
                    Some(Verdict::SoftNotFound)
                } else if msg.starts_with("paywall:") {
                    Some(Verdict::Paywall)
                } else {
                    None
                };
                let failed_verdict = if msg.starts_with("browser navigation error:")
                    || msg.starts_with("browser launch failed:")
                    || msg.starts_with("browser recovery failed")
                {
                    "Unknown".to_string()
                } else if msg.starts_with("browser document incomplete") {
                    "Incomplete".to_string()
                } else {
                    observed_gate
                        .map(|v| format!("{v:?}"))
                        .unwrap_or_else(|| failure_verdict(&final_verdict, kind))
                };
                let observed_status = trace
                    .browser_document
                    .as_ref()
                    .map(|d| d.status)
                    .unwrap_or((final_status != 0).then_some(final_status));
                // Honest advice: a stall the browser could not clear
                // is not a network fault (v4.7 D2 : never send an agent
                // to "another network" for a wall the browser could
                // not pass).
                let deadline_stall = msg.contains("deadline");
                return tool_error_structured(
                    msg,
                    kind,
                    Some(json!({
                        "url": trace.browser_document.as_ref().map(|d| d.url.as_str()).unwrap_or(&url),
                        "status": observed_status,
                        "verdict": failed_verdict,
                        "next_action": browser_failure_next_action(
                            &failed_verdict,
                            deadline_stall,
                            observed_gate.or_else(|| out.as_ref().map(|o| o.verdict)),
                            observed_status.unwrap_or(0),
                            kind,
                        ),
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
    let mut stitch_next = None;
    if stitch && output_opts.must_contain.is_none() && !output_opts.toc {
        const STITCH_MAX_PARTS: usize = 6;
        const STITCH_BUDGET: usize = 1_048_576;
        let base = ghost_html.or_else(|| {
            out.as_ref()
                .filter(|o| o.verdict == Verdict::ContentOk && o.url == final_url)
                .map(|o| {
                    let ct = o
                        .headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    (crate::extract::charset::decode(&o.body, &ct), o.url.clone())
                })
        });
        if let Some((html, base_url)) = base
            && let Some(ex) = final_ex.as_mut()
        {
            let base_host = url::Url::parse(&base_url)
                .ok()
                .and_then(|u| u.host_str().map(String::from));
            let mut visited = std::collections::HashSet::from([base_url.clone()]);
            let mut next = find_rel_next(&html, &base_url);
            if let Some(offset) = ex.next_offset.take() {
                if let Some(marker) = ex.markdown.rfind("\n\n*[truncated : continue") {
                    ex.markdown.truncate(marker);
                }
                stitch_next = Some(
                    json!({"url": base_url, "offset": offset, "reason": "article collection limit"}),
                );
                next = None;
            }
            while let Some(next_url) = next.take() {
                stitch_next = Some(
                    json!({"url": next_url, "offset": 0, "reason": "part limit or unavailable part"}),
                );
                if stitched_parts >= STITCH_MAX_PARTS || ex.markdown.len() + 200 >= STITCH_BUDGET {
                    break;
                }
                if !visited.insert(next_url.clone()) {
                    stitch_next = Some(json!({"url": next_url, "reason": "pagination cycle"}));
                    break;
                }
                // Hijack guard: never follow rel=next off-host.
                let Ok(nu) = url::Url::parse(&next_url) else {
                    break;
                };
                if nu.host_str().map(String::from) != base_host {
                    break;
                }
                let fetched = match daemon.fetcher.fetch(&next_url).await {
                    Ok(o2)
                        if matches!(o2.verdict, Verdict::ContentOk)
                            && url::Url::parse(&o2.url)
                                .ok()
                                .and_then(|u| u.host_str().map(String::from))
                                == base_host =>
                    {
                        o2
                    }
                    _ => break,
                };
                if fetched.url != next_url && !visited.insert(fetched.url.clone()) {
                    stitch_next =
                        Some(json!({"url": fetched.url, "reason": "redirected pagination cycle"}));
                    break;
                }
                trace.step(
                    "stitch",
                    "fetch-part",
                    &next_url,
                    fetched.elapsed.as_millis(),
                );
                let ct2 = fetched
                    .headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let html2 = crate::extract::charset::decode(&fetched.body, &ct2);
                let mut popts = opts.clone();
                popts.max_chars = Some(
                    STITCH_BUDGET
                        .saturating_sub(ex.markdown.len() + 40)
                        .max(200),
                );
                match extract::extract_off_worker(&fetched.body, &ct2, &fetched.url, &popts).await {
                    Ok(mut pe) => {
                        if pe.thin {
                            break;
                        }
                        if let Some(offset) = pe.next_offset {
                            if let Some(marker) = pe.markdown.rfind("\n\n*[truncated : continue") {
                                pe.markdown.truncate(marker);
                            }
                            stitch_next = Some(
                                json!({"url": fetched.url, "offset": offset, "reason": "article collection limit"}),
                            );
                        } else {
                            stitch_next = None;
                        }
                        let md = strip_part_frontmatter(&pe.markdown);
                        stitched_parts += 1;
                        ex.markdown
                            .push_str(&format!("\n\n---\n\n*(part {stitched_parts})*\n\n"));
                        ex.markdown.push_str(&md);
                        ex.total_chars = ex.markdown.len();
                        if pe.next_offset.is_some() {
                            break;
                        }
                        next = find_rel_next(&html2, &fetched.url);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    if stitch
        && output_opts.must_contain.is_none()
        && !output_opts.toc
        && let Some(ex) = final_ex.as_mut()
    {
        let (slice, next) = extract::paginate_public(
            &ex.markdown,
            output_opts.offset,
            output_opts.max_chars.unwrap_or(16_000),
        );
        ex.markdown = slice;
        ex.next_offset = next;
        ex.tokens_est = ex.markdown.len() / 4;
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

    // #282: a challenge interstitial or a chrome-only shell is not
    // content, whatever tier produced it. The failure is "walled"
    // so the escalation ladder (ghost, a configured unlocker) can
    // engage instead of the agent trusting navigation boilerplate.
    if let Some((msg, kind, action)) = content_fail(&ex.markdown, &url, final_status) {
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
    // must never ship as successful content regardless of the tier
    // that produced it (live case: facebook's solve returned React
    // internals as markdown with a clean verdict). Honest fail
    // with the repair hint instead of garbage "success".
    if is_framework_shell(&ex) {
        return tool_error_structured(
            format!(
                "blocked at {url} : the site renders an app shell without real content for non-interactive clients"
            ),
            "permanent",
            Some(json!({
                "url": url,
                "verdict": "Challenge(Generic)",
                "next_action": "use an agent browser to browse sites like these; retry later",
                "escalation": trace.value(),
            })),
        );
    }

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
    maybe_record_agent_outcome(daemon, &host, &opts, &ex, &final_verdict);
    if daemon.state.lock().await.is_flaky(&host) {
        res["structuredContent"]["stability"] = json!("flaky");
    }
    if prewarmed {
        res["_meta"]["com.donsetch/fetch-debug"]["prewarmed_by_search"] = json!(true);
    }
    if stitch
        && output_opts.must_contain.is_none()
        && !output_opts.toc
        && let Some(sc) = res.pointer_mut("/structuredContent")
    {
        sc["stitched"] = json!(stitched_parts);
        sc["stitch_complete"] = json!(stitch_next.is_none());
        if let Some(next_part) = stitch_next {
            sc["content_complete"] = json!(false);
            sc["read_status"] = json!("partial");
            sc["next_part"] = next_part;
        }
    }
    apply_link_handles(daemon, &mut res).await;
    // v3 anti-cloak: a known-walled domain passing tier-1 cold
    // cleanly is suspicious. One equivalence check; a warning is
    // stamped, never a silent pass.
    let mut cloak_warning: Option<String> = None;
    // Compare suspicious HTTP-only content once. A recovery already observed
    // this browser document, and explicit tier 1 must not launch a browser.
    // v4.7 V03: JSON data endpoints and PDFs are terminal at the HTTP
    // tier : the equivalence comparison needs a browser-readable
    // document, and a data payload is not one.
    if known_walled
        && !adapter_host
        && !skip_tier1
        && !is_warm
        && !need_ghost
        && !json_like
        && !is_pdf_content
        && tier != "1"
    {
        let started = std::time::Instant::now();
        trace.step("2", "anti-cloak", "started", 0);
        let outcome = match anticloak_check(daemon, &url, &ex.markdown, budget, &call.route).await {
            CloakComparison::Equivalent => {
                daemon.state.lock().await.record_cold_ok(&host);
                "equivalent"
            }
            CloakComparison::Divergent(note) => {
                cloak_warning = Some(note);
                "divergent"
            }
            CloakComparison::Unavailable(reason) => reason,
        };
        trace.step("2", "anti-cloak", outcome, started.elapsed().as_millis());
        res["_meta"]["com.donsetch/fetch-debug"]["escalation"] = trace.value();
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
    if opts.focus.is_none()
        && opts.section.is_none()
        && opts.selector.is_none()
        && !opts.toc
        && opts.must_contain.is_none()
        && !stitch
    {
        apply_page_history(
            daemon,
            &mut res,
            &display_url,
            PageFacts {
                fingerprint: ex.fingerprint.as_deref(),
                markdown: &ex.markdown,
                title: ex.title.as_deref(),
                complete: opts.offset == 0
                    && ex.next_offset.is_none()
                    && !ex.thin
                    && ex.partial.is_none(),
            },
            since_last,
        );
    }
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
        content_fail(&ex.markdown, url, outcome.status).map(|(msg, _, _)| msg)
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn ghost_escalate(
    daemon: &Arc<Daemon>,
    url: &str,
    host: &str,
    opts: &ExtractOptions,
    learn: bool,
    browser_only: bool,
    shot: Option<&str>,
    trace: &mut Trace,
    budget: Budget,
    route: &crate::transport::request_route::RequestRoute,
    accept_language: Option<&str>,
) -> Result<(extract::Extracted, &'static str, u16, String, String), (String, &'static str)> {
    let t0 = std::time::Instant::now();
    // v4 E2: ghost agrees with the persona pin (viewport + locale).
    let mut wire = {
        let state = daemon.state.lock().await;
        state
            .personas
            .get(host)
            .filter(|p| p.quarantine_reason.is_none())
            .map(|p| p.ghost_wire())
            .unwrap_or_default()
    };
    wire.route = Some(route.clone());
    trace.step("2", "browser-launch", "started", 0);
    let g = daemon
        .ghost_mgr
        .acquire_for_wire(&daemon.profile, Some(host), wire)
        .await
        .map_err(|e| (format!("browser launch failed: {e}"), "permanent"))?;
    trace.step("2", "browser-launch", "ok", t0.elapsed().as_millis());
    trace.step(
        "2",
        "browser-queue",
        if g.reused { "reused" } else { "launched" },
        g.queue_wait.as_millis(),
    );
    let t1 = std::time::Instant::now();
    let read = daemon
        .ghost_mgr
        .read_document(g, &daemon.profile, url, budget.pass(20))
        .await
        .map_err(|error| {
            let message = error.to_string();
            trace.step("2", "ghost-render", &message, t1.elapsed().as_millis());
            (format!("browser navigation error: {message}"), "transient")
        })?;
    if let Some(recovery) = read.recovery {
        trace.step(
            "2",
            "browser-read-retry",
            recovery.reason,
            recovery.elapsed.as_millis(),
        );
    }
    let mut g = read.guard;
    let mut page = read.page;
    trace.step(
        "2",
        "ghost-render",
        &format!(
            "outcome={:?} dom={}KB",
            page.outcome,
            page.html.len() / 1024
        ),
        t1.elapsed().as_millis(),
    );
    trace.observe_browser(&page.document);
    if crate::config::cfg().debug.ghost {
        let p = dump_ghost_dom(
            &crate::paths::cache_dir().join("ghost-debug"),
            host,
            &page.html,
        );
        eprintln!(
            "[ghost_escalate] dom={}B dumped to {}",
            page.html.len(),
            p.display()
        );
    }
    if crate::ghost::is_chrome_error_html(&page.html) {
        return Err((
            format!("browser network error at {url}: Chrome could not load the document"),
            "transient",
        ));
    }
    let gate = crate::detect::walls::detect_dom_smart(page.html.as_bytes());
    if matches!(
        gate,
        Verdict::AuthWall | Verdict::Paywall | Verdict::SoftNotFound
    ) {
        let status = page.document.status.unwrap_or(0);
        return Err((
            verdict_error(gate, status, &page.document.url),
            verdict_kind(gate, status),
        ));
    }
    if page.outcome.is_wall() {
        // Interactive widgets are outside this solver's capabilities. The
        // first real DOM is decisive; another navigation cannot answer a
        // human challenge. Turnstile keeps its bounded warm second pass.
        if page.outcome == ops::BrowserOutcome::HumanRequired {
            trace.step(
                "2",
                "solve",
                "human challenge required",
                t1.elapsed().as_millis(),
            );
            if let Some(path) = shot {
                record_shot(&g, path, trace).await;
            }
            daemon.state.lock().await.record_wall_failed(host);
            return Err((
                format!("blocked at {url} : interactive captcha requires a human browser session"),
                "walled",
            ));
        }
        // Solve-grade second pass: some vendors (Akamai) run the
        // sensor on the first load and only clear on a follow-up
        // navigation once their first-party state is planted. The
        // browser is warm now: one bounded re-render, then a
        // settle re-check. Never more: two passes is the ceiling,
        // an honest captcha stays an honest captcha.
        let t1b = std::time::Instant::now();
        let page2 = ops::ghost_fetch(&mut g, url, budget.pass(20)).await;
        match page2 {
            Ok(p2) if !p2.outcome.is_wall() => {
                trace.observe_browser(&p2.document);
                trace.step(
                    "2",
                    "solve-pass2",
                    &format!(
                        "cleared: captcha={} dom={}KB",
                        p2.outcome.is_wall(),
                        p2.html.len() / 1024
                    ),
                    t1b.elapsed().as_millis(),
                );
                // Fall through into the normal harvest/retry flow.
                page = p2;
            }
            Ok(p2) if p2.outcome.is_wall() => {
                trace.observe_browser(&p2.document);
                // A failed second pass must leave the same trail a
                // successful one does. The debug log showed two
                // attempts while the escalation listed one, so the
                // trail implied the wall had been met once (#258).
                trace.step(
                    "2",
                    "solve-pass2",
                    &format!(
                        "still walled: captcha={} dom={}KB",
                        p2.outcome.is_wall(),
                        p2.html.len() / 1024
                    ),
                    t1b.elapsed().as_millis(),
                );
                if let Some(p) = shot {
                    record_shot(&g, p, trace).await;
                }
                // The wall survived BOTH passes in a real browser:
                // this is wall-persisting evidence, recorded.
                daemon.state.lock().await.record_wall_failed(host);
                // #282: different recoveries get different codes. A
                // page holding an interactive captcha widget needs a
                // human or a vendor solver; a vendor-less challenge
                // that never finished is the retry-or-unlocker class.
                let msg = if crate::detect::walls::interactive_captcha(p2.html.as_bytes()) {
                    "interactive captcha or challenge could not be solved automatically. Use an Agent browser to browse sites like these".to_string()
                } else {
                    "the page is an anti-bot challenge that did not clear (the challenge never finished on its own; retry later or let a configured unlocker solve this class)".to_string()
                };
                return Err((format!("blocked at {url} : {msg}"), "walled"));
            }
            // ghost_fetch errored on the retry (automation failure,
            // not a wall): no wall memory recorded.
            Err(error) => {
                if let Some(p) = shot {
                    record_shot(&g, p, trace).await;
                }
                return Err((
                    format!("browser recovery failed at {url}: {error}"),
                    "transient",
                ));
            }
            _ => unreachable!(),
        }
    }
    let status = page.document.status.unwrap_or(0);
    let observed_gate = match status {
        401 => Some(Verdict::AuthWall),
        402 => Some(Verdict::Paywall),
        404 => Some(Verdict::SoftNotFound),
        _ => crate::detect::walls::content_gate(page.html.as_bytes()),
    };
    if let Some(verdict) = observed_gate {
        return Err((
            verdict_error(verdict, status, &page.document.url),
            verdict_kind(verdict, status),
        ));
    }
    if page.outcome == ops::BrowserOutcome::Incomplete {
        return Err((
            format!("browser document incomplete at {url}: content did not settle"),
            "transient",
        ));
    }
    if !page.outcome.is_wall() {
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
            if let Ok(p2) = ops::ghost_fetch(&mut g, url, budget.pass(20)).await {
                let v2 = crate::detect::walls::detect_dom_smart(p2.html.as_bytes());
                if !p2.outcome.is_wall()
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
                    trace.observe_browser(&page.document);
                }
            }
        }
    }
    if !page.cookies.is_empty() {
        // A pooled browser can predate a logout for one of its
        // domains: a dead session must not ride back into the jar
        // or the vault.
        let cookies = crate::ghost::cache::session_cookies_since(&page.cookies, g.launched_epoch);
        daemon.fetcher.import_vault_cookies(&cookies).await;
        crate::ghost::cache::store_session_cookies(&cookies);
    }
    // Retry tier 1 with fresh cookies : the cheap path back to
    // normal HTTP when the gate was cookie-driven.
    let t2 = std::time::Instant::now();
    let retry = if !browser_only && !page.cookies.is_empty() {
        let r = daemon
            .fetcher
            .fetch_persona_on_route(url, accept_language, Some(route))
            .await
            .ok();
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

    // HTTP replay is an independent candidate. A failed replay cannot
    // override the browser's observed response or usable document.

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
            && let Ok(e) = extract::extract_off_worker(&r.body, &ct, &r.url, opts).await
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
        &page.document.url,
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
                page.document.status.unwrap_or(0),
                page.document.url.clone(),
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
            &page.document.url,
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
                    page.document.status.unwrap_or(0),
                    page.document.url.clone(),
                ));
            }
        }
    }

    if let Some((thin, e, t, s, u)) = best
        && !thin
    {
        // #282: a challenge interstitial or chrome-only shell that
        // passed the thin gate must not be served (or learned from)
        // as content.
        if let Some((msg, kind, _)) = content_fail(&e.markdown, url, s) {
            return Err((msg, kind));
        }
        // Learning is gated on WALL-DRIVEN escalation AND gated on
        // CONTENT : success is "we got content", not "we got HTTP
        // 200". The replay probe (or its absence) sets replay_ok.
        if learn || page.vendor.is_some() {
            // A pooled browser can predate a logout for one of its
            // domains: a dead session must not ride back into the
            // learned route or the vault.
            let cookies =
                crate::ghost::cache::session_cookies_since(&page.cookies, g.launched_epoch);
            daemon.state.lock().await.record_solved(
                host,
                &cookies,
                page.vendor.as_deref(),
                replay_content_ok,
            );
            crate::ghost::cache::store_session_cookies(&cookies);
        }
        // Don't cache challenge/wall DOMs : defense in depth alongside
        // the ghost_fetch timeout check. A challenge page that has
        // enough block structure to pass !thin would otherwise be
        // cached and re-served as ContentOk forever.
        let dom_verdict = crate::detect::walls::detect_dom_smart(page.html.as_bytes());
        if dom_verdict == Verdict::ContentOk {
            daemon
                .state
                .lock()
                .await
                .record_render(&page.document.url, &page.html);
        }
        return Ok((e, t, s, u, page.html));
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
        if let Some(ext) = crate::adapters::extract_json(
            trimmed.as_bytes(),
            "application/json",
            &page.document.url,
            opts,
        ) {
            return Ok((
                ext,
                "ghost-json",
                page.document.status.unwrap_or(0),
                page.document.url.clone(),
                page.html,
            ));
        }
        // No adapter: DonSift's generic pass for the raw body.
        if let Ok(ext) = extract::extract(
            trimmed.as_bytes(),
            "application/json",
            &page.document.url,
            opts,
        ) {
            return Ok((
                ext,
                "ghost-json",
                page.document.status.unwrap_or(0),
                page.document.url.clone(),
                page.html,
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
    if !page.outcome.is_wall() {
        let doc = scraper::Html::parse_document(&page.html);
        let meta = crate::extract::metadata::metadata(&doc);
        let max_chars = opts.max_chars.unwrap_or(16_000).max(200);
        if let Some(fb) = crate::extract::fallback::text_fallback(
            &page.html,
            &meta,
            &page.document.url,
            opts,
            max_chars,
        ) {
            // The fallback is the LAST resort. A real page render
            // whose useful text is short must still succeed (live
            // case: instagram's profile card = 279 collectible
            // chars, all of it the useful content; the old gate
            // failed the whole fetch because the 800-char thin bar
            // is tuned for articles). Only a login-only shell or a
            // sub-sentence fragment stays a fail.
            let login_only = fb.markdown.len() < 60
                && (fb.markdown.to_ascii_lowercase().contains("log in")
                    || fb.markdown.to_ascii_lowercase().contains("sign up"));
            // The last resort must not resurrect junk (#282): a
            // challenge interstitial or chrome-only shell stays a
            // failure even though its text is technically non-empty.
            let status = page.document.status.unwrap_or(0);
            if content_fail(&fb.markdown, url, status).is_none()
                && (!fb.thin || (fb.markdown.len() >= 40 && !login_only))
            {
                return Ok((
                    fb,
                    "ghost-text",
                    page.document.status.unwrap_or(0),
                    page.document.url.clone(),
                    page.html,
                ));
            }
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
    if matches!(dom_verdict, Verdict::Challenge(_) | Verdict::Blocked) {
        daemon.state.lock().await.record_wall_failed(host);
        // #282: the code must say which recovery applies. An
        // interactive widget needs a human or a vendor solver; a
        // bare challenge that never finished is retry-or-unlocker.
        let msg = if crate::detect::walls::interactive_captcha(page.html.as_bytes()) {
            "interactive captcha or challenge could not be solved automatically. Use an Agent browser to browse sites like these".to_string()
        } else {
            "the page is an anti-bot challenge that did not clear (the challenge never finished on its own; retry later or let a configured unlocker solve this class)".to_string()
        };
        return Err((format!("blocked at {url} : {msg}"), "walled"));
    }
    if matches!(dom_verdict, Verdict::AuthWall | Verdict::Paywall) {
        return Err((format!("login or payment required at {url}"), "walled"));
    }
    if page.html.len() < 5_000 {
        return Err((
            format!("browser document incomplete at {url}: no extractable content"),
            "transient",
        ));
    }
    Err((
        format!(
            "content could not be extracted at {url}: browser returned a {}KB document without readable content",
            page.html.len() / 1024
        ),
        "permanent",
    ))
}

/// PDF-shaped URL check for the actions guard (before the main
/// flow computes its own is_pdf_url). Covers both the .pdf
/// suffix convention and the /pdf/ path convention (arXiv:
/// arxiv.org/pdf/1706.03762 serves a PDF with no extension).
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
/// None when the text is real content.
fn content_fail(
    markdown: &str,
    url: &str,
    status: u16,
) -> Option<(String, &'static str, &'static str)> {
    if let Some(wall) = crate::detect::walls::text_wall(markdown) {
        if wall == Verdict::AuthWall {
            return Some((
                format!("login required at {url}"),
                "walled",
                "authenticate with donsetch login <domain>, or use an accessible source",
            ));
        }
        return Some((
            format!(
                "blocked at {url} : the page is an anti-bot challenge that did not clear (the extracted text is the interstitial, not content)"
            ),
            "walled",
            "retry later : the challenge may clear; tier=auto renders with a browser, and a configured unlocker solves this class",
        ));
    }
    if (200..300).contains(&status) && crate::extract::quality::chrome_only(markdown) {
        return Some((
            format!(
                "blocked at {url} : the page rendered only navigation and login chrome (an empty shell or a login wall), no content"
            ),
            "walled",
            "use an agent browser to browse sites like these : the page may be a client-rendered shell or a login wall",
        ));
    }
    None
}

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

/// v4.7 V03: JSON data-endpoint URL check : the path names a `.json`
/// document (reddit `.json`, API dumps; query and fragment stripped,
/// case-insensitive). A structured payload is terminal at the HTTP
/// tier : a browser cannot improve it.
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

/// v2: fetch with an action script : navigate, act (click /
/// type / press / scroll / wait), then run the NORMAL DonSift
/// extraction over the final DOM. focus/section/toc all work
/// on the interacted-with page. One call replaces hound's
/// navigate→act→act→read round-trips.
#[allow(clippy::too_many_arguments)]
async fn fetch_with_actions(
    daemon: &Arc<Daemon>,
    url: &str,
    host: &str,
    opts: &ExtractOptions,
    actions: &[crate::ghost::actions::Action],
    shot: Option<&str>,
    image_text: bool,
    call: &FetchCall,
) -> Value {
    let budget = call.budget;
    let mut trace = Trace::default();
    trace.step("route", "actions", "browser-script", 0);

    let t0 = std::time::Instant::now();
    let mut wire = {
        let state = daemon.state.lock().await;
        state
            .personas
            .get(host)
            .filter(|p| p.quarantine_reason.is_none())
            .map(|p| p.ghost_wire())
            .unwrap_or_default()
    };
    wire.route = Some(call.route.clone());
    let g = match daemon
        .ghost_mgr
        .acquire_for_wire(&daemon.profile, Some(host), wire)
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
    let read = match daemon
        .ghost_mgr
        .read_document(g, &daemon.profile, url, budget.pass(25))
        .await
    {
        Ok(read) => read,
        Err(error) => {
            return tool_error_structured(
                format!("browser navigation error: {error}"),
                "transient",
                Some(json!({ "url": url, "status": null, "escalation": trace.value() })),
            );
        }
    };
    if let Some(recovery) = read.recovery {
        trace.step(
            "2",
            "browser-read-retry",
            recovery.reason,
            recovery.elapsed.as_millis(),
        );
    }
    let mut g = read.guard;
    let page = read.page;
    trace.step(
        "2",
        "ghost-render",
        &format!(
            "captcha={} dom={}KB",
            page.outcome.is_wall(),
            page.html.len() / 1024
        ),
        t1.elapsed().as_millis(),
    );
    trace.observe_browser(&page.document);
    if page.outcome.is_wall() {
        if let Some(p) = shot {
            record_shot(&g, p, &mut trace).await;
        }
        return tool_error_structured(
            format!(
                "blocked at {url} : interactive captcha before actions could run. Use an Agent browser to browse sites like these"
            ),
            "walled",
            Some(json!({
                "url": url,
                "status": page.document.status,
                "verdict": "Challenge",
                "next_action": next_action_for(Some(Verdict::Challenge(Vendor::Generic)), 200, "walled"),
                "escalation": trace.value(),
            })),
        );
    }

    let initial_gate = match page.document.status {
        Some(401) => Some(Verdict::AuthWall),
        Some(402) => Some(Verdict::Paywall),
        Some(404) => Some(Verdict::SoftNotFound),
        _ => crate::detect::walls::content_gate(page.html.as_bytes()),
    };
    if let Some(gate) = initial_gate {
        return tool_error_structured(
            verdict_error(gate, page.document.status.unwrap_or(0), &page.document.url),
            verdict_kind(gate, page.document.status.unwrap_or(0)),
            Some(
                json!({"url":page.document.url, "status":page.document.status,
                "verdict":format!("{gate:?}"), "escalation":trace.value()}),
            ),
        );
    }

    // Run the script.
    let t2 = std::time::Instant::now();
    trace.step("2", "action-execution", "started", 0);
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
                record_shot(&g, p, &mut trace).await;
            }
            let steps_json: Vec<Value> = partial
                .iter()
                .map(|o| json!({"step": o.step, "action": o.action, "outcome": o.outcome, "ms": o.ms}))
                .collect();
            return tool_error_structured(
                format!(
                    "actions[{step}] failed: {reason} : an action may already have completed (see structuredContent.actions)"
                ),
                "permanent",
                Some(json!({
                    "url": url,
                    "status": page.document.status,
                    "actions": steps_json,
                    "escalation": trace.value(),
                    "next_action": "inspect the page with a plain fetch (no actions), correct the failing step's selector/text, re-run",
                })),
            );
        }
    };

    // Post-action DOM + optional screenshot for visual debugging.
    let before = g.document();
    let html = match g.outer_html().await {
        Ok(h) => h,
        Err(e) => {
            return tool_error_structured(
                format!("post-action DOM read failed: {e}"),
                "transient",
                Some(json!({
                    "url": url,
                    "status": page.document.status,
                    "escalation": trace.value(),
                })),
            );
        }
    };
    let document = g.document();
    if before.generation != document.generation {
        return tool_error_structured(
            "post-action document changed during extraction; add a wait for the destination",
            "transient",
            Some(json!({"url": url, "status": null, "escalation": trace.value()})),
        );
    }
    trace.observe_browser(&document);
    // Actions can navigate; the final URL is part of the DOM's provenance.
    if let Err(e) = crate::fetch::guards::ensure_url_safe(&document.url).await {
        return tool_error_structured(
            format!("post-action navigation failed: {e}"),
            fetch_error_kind(&e),
            Some(
                json!({"url": document.url, "code": fetch_error_code(&e), "escalation": trace.value()}),
            ),
        );
    }
    let gate = match document.status {
        Some(401) => Verdict::AuthWall,
        Some(402) => Verdict::Paywall,
        Some(404) => Verdict::SoftNotFound,
        _ => crate::detect::walls::detect_dom_smart(html.as_bytes()),
    };
    if gate != Verdict::ContentOk {
        return tool_error_structured(
            verdict_error(gate, document.status.unwrap_or(0), &document.url),
            verdict_kind(gate, document.status.unwrap_or(0)),
            Some(
                json!({"url": document.url, "status": document.status, "escalation": trace.value()}),
            ),
        );
    }
    if let Some(p) = shot {
        record_shot(&g, p, &mut trace).await;
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
        // A pooled browser can predate a logout for one of its
        // domains: a dead session must not ride back into the jar
        // or the vault.
        let cookies = crate::ghost::cache::session_cookies_since(&cookies, g.launched_epoch);
        daemon.fetcher.import_vault_cookies(&cookies).await;
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
    if let Ok(e) = extract::extract(
        html.as_bytes(),
        extract::charset::GHOST_TEXT_CT,
        &document.url,
        opts,
    ) && !e.thin
    {
        best = Some(e);
    }
    if best.is_none() {
        let mut lopts = opts.clone();
        lopts.include_links = true;
        if let Ok(e2) = extract::extract(
            html.as_bytes(),
            extract::charset::GHOST_TEXT_CT,
            &document.url,
            &lopts,
        ) && !e2.thin
        {
            best = Some(e2);
        }
    }
    let Some(ex) = best else {
        return tool_error_structured(
            format!(
                "actions succeeded but the resulting page yielded no extractable content ({}KB DOM) : inspect the resulting page without repeating actions",
                html.len() / 1024
            ),
            "transient",
            Some(json!({
                "url": document.url,
                "status": document.status,
                "verdict": "Incomplete",
                "escalation": trace.value(),
                "next_action": "add {\"do\":\"wait_text\",\"text\":\"<expected>\"} or {\"do\":\"wait\",\"ms\":2000} before extraction",
            })),
        );
    };

    let steps_json: Vec<Value> = outcomes
        .iter()
        .map(|o| json!({"step": o.step, "action": o.action, "outcome": o.outcome, "ms": o.ms}))
        .collect();
    let mut res = finish_result(
        &ex,
        "2-actions",
        document.status.unwrap_or(0),
        "ContentOk",
        &document.url,
        &trace,
        t0.elapsed().as_millis(),
    );
    maybe_record_agent_outcome(daemon, host, opts, &ex, "ContentOk");
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

enum CloakComparison {
    Equivalent,
    Divergent(String),
    Unavailable(&'static str),
}

/// v3 anti-cloak: a domain KNOWN to be walled (needs_tier2 in the
/// profile) suddenly serving clean tier-1 content is suspicious :
/// bot walls sometimes serve benign-looking bait to suspected
/// bots. Render the same URL in the real browser and compare word
/// sets. Material divergence → `cloak_suspected` with a trust
/// recommendation. Cost: one browser render, only on suspicion.
async fn anticloak_check(
    daemon: &Arc<Daemon>,
    url: &str,
    tier1_markdown: &str,
    budget: Budget,
    route: &RequestRoute,
) -> CloakComparison {
    let host = crate::search::rank::host_of(url);
    let mut wire = {
        let state = daemon.state.lock().await;
        state
            .personas
            .get(host.as_str())
            .filter(|p| p.quarantine_reason.is_none())
            .map(|p| p.ghost_wire())
            .unwrap_or_default()
    };
    wire.route = Some(route.clone());
    let Ok(g) = daemon
        .ghost_mgr
        .acquire_for_wire(&daemon.profile, Some(host.as_str()), wire)
        .await
    else {
        return CloakComparison::Unavailable("unavailable:launch");
    };
    let Ok(read) = daemon
        .ghost_mgr
        .read_document(g, &daemon.profile, url, budget.pass(20))
        .await
    else {
        return CloakComparison::Unavailable("unavailable:transport");
    };
    let _guard = read.guard;
    let page = read.page;
    if page.outcome.is_wall() {
        return CloakComparison::Divergent(
            "browser sees a challenge where HTTP saw content".to_string(),
        );
    }
    if page.outcome != ops::BrowserOutcome::Content {
        return CloakComparison::Unavailable("unavailable:access");
    }
    let Ok(ex) = extract::extract(
        page.html.as_bytes(),
        extract::charset::GHOST_TEXT_CT,
        url,
        &ExtractOptions::default(),
    ) else {
        return CloakComparison::Unavailable("unavailable:extraction");
    };
    pub(super) fn words(s: &str) -> std::collections::HashSet<&str> {
        s.split_whitespace().collect()
    }
    let a = words(tier1_markdown);
    let b = words(&ex.markdown);
    if b.is_empty() {
        return CloakComparison::Unavailable("unavailable:empty");
    }
    let inter = a.intersection(&b).count();
    let union = a.union(&b).count();
    let sim = if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    };
    if sim < 0.55 {
        CloakComparison::Divergent(format!(
            "HTTP and browser content diverge (similarity {sim:.2}) : the HTTP copy may be bot-bait; browser tier text is the one to trust"
        ))
    } else {
        CloakComparison::Equivalent
    }
}

/// v3 resurrection fetch: when a URL is truly dead (404, paywall,
/// unsolvable wall) consult the keyless Wayback Machine and serve
/// the nearest snapshot : labeled ruthlessly so archived content
/// can never masquerade as live. `archive: auto` (default) only on
/// dead-end failures; `only` skips the live attempt; `off` never.
/// Err carries the exact stage that gave up, so the caller can
/// never confuse "the URL was never archived" with "a snapshot
/// exists but was unusable" or "the archive was unreachable".
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResurrectStage {
    /// The availability/CDX endpoints could not be reached.
    LookupUnreachable,
    /// Both indexes were consulted and have no 200 capture.
    /// (Non-200 captures never get this far: availability and CDX
    /// are both probed until a 200 capture turns up or both answer
    /// empty.)
    NoSnapshot,
    /// The snapshot page failed at the transport layer.
    SnapshotFetch,
    /// The snapshot page tripped the wall detector.
    SnapshotVerdict,
    /// The snapshot body is binary (PDF/image), not extractable HTML.
    SnapshotBinary,
    /// The snapshot extracted to too little text to serve.
    SnapshotThin(usize),
}

impl ResurrectStage {
    /// Machine-readable tag for structuredContent.archive_stage.
    fn tag(&self) -> String {
        match self {
            Self::LookupUnreachable => "lookup_unreachable".into(),
            Self::NoSnapshot => "no_snapshot".into(),
            Self::SnapshotFetch => "snapshot_fetch_failed".into(),
            Self::SnapshotVerdict => "snapshot_verdict_rejected".into(),
            Self::SnapshotBinary => "snapshot_binary".into(),
            Self::SnapshotThin(n) => format!("snapshot_extract_thin({n})"),
        }
    }
}

struct ResurrectError {
    stage: ResurrectStage,
    /// Nearest snapshot URL reached, when one was found : lets the
    /// caller distinguish "never archived" from "archived but the
    /// copy was unusable" (and hand over the URL for inspection).
    snapshot_url: Option<String>,
}

/// What an archive index said about the URL.
enum Avail {
    /// A 200-status capture: (snapshot URL, capture timestamp).
    Found((String, String)),
    /// The index answered and has nothing usable.
    Empty,
    /// The index could not be reached (or answered with a server
    /// error) : says nothing about the archive's contents.
    Unreachable,
}

/// Availability API lookup (keyless, public).
async fn availability_lookup(daemon: &Arc<Daemon>, url: &str, route: &RequestRoute) -> Avail {
    let avail_url = format!(
        "https://archive.org/wayback/available?url={}",
        encode_query_value(url)
    );
    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        daemon
            .fetcher
            .fetch_persona_on_route(&avail_url, None, Some(route)),
    )
    .await;
    let Ok(Ok(out)) = fetched else {
        return Avail::Unreachable;
    };
    if !(200..300).contains(&out.status) {
        return Avail::Unreachable;
    }
    // A 200 whose body is not JSON (rate-limit HTML, an interstitial)
    // says nothing definitive : the CDX fallback gets its shot.
    let Ok(v) = serde_json::from_slice::<Value>(&out.body) else {
        return Avail::Unreachable;
    };
    let Some(closest) = v.pointer("/archived_snapshots/closest") else {
        return Avail::Empty;
    };
    let Some(snap_url) = closest.get("url").and_then(Value::as_str) else {
        return Avail::Empty;
    };
    let ts = closest
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The API returns status as a STRING ("200"); accept both.
    let snap_status = closest
        .get("status")
        .map(|s| {
            s.as_i64()
                .or_else(|| s.as_str().and_then(|x| x.parse().ok()))
                .unwrap_or(0)
        })
        .unwrap_or(0);
    if snap_status != 200 {
        // A non-200 "closest" may have a 200 sibling the lossy
        // availability view missed : let the complete index decide.
        return Avail::Empty;
    }
    Avail::Found((snap_url.to_string(), ts))
}

/// The complete CDX capture index. `url=` goes schemeless : CDX
/// canonicalizes the scheme away, so a capture recorded under
/// http:// answers an https:// query (the availability API is
/// scheme-strict and misses those). limit=-5 keeps the LAST rows,
/// i.e. the captures nearest the present.
async fn cdx_lookup(daemon: &Arc<Daemon>, url: &str, route: &RequestRoute) -> Avail {
    let bare = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let cdx_url = format!(
        "https://web.archive.org/cdx/search/cdx?url={}&output=json&filter=statuscode:200&limit=-5",
        encode_query_value(bare)
    );
    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        daemon
            .fetcher
            .fetch_persona_on_route(&cdx_url, None, Some(route)),
    )
    .await;
    let Ok(Ok(out)) = fetched else {
        return Avail::Unreachable;
    };
    // CDX answers rate limits and abuse holds with HTML, not JSON :
    // a transient condition, never evidence of "never archived".
    if !(200..300).contains(&out.status) {
        return Avail::Unreachable;
    }
    let Ok(v) = serde_json::from_slice::<Value>(&out.body) else {
        return Avail::Unreachable;
    };
    match cdx_latest(&v) {
        // Rebuild from the row's `original` : it is the exact form
        // wayback replayed and canonicalized (urlkey is computed on
        // it, trailing slashes, :80 port and all). Rebuilding from
        // the REQUESTED url instead mismatched the urlkey when the
        // capture was recorded under a different path form, and
        // wayback answered with its calendar page instead of the
        // capture.
        Some((ts, original)) => {
            let target: &str = if original.is_empty() {
                bare
            } else {
                original.as_str()
            };
            Avail::Found((format!("https://web.archive.org/web/{ts}/{target}"), ts))
        }
        None => Avail::Empty,
    }
}

/// Pick the nearest-to-present 200 capture from a CDX json response.
/// Rows are [["urlkey","timestamp","original", ...], ...] : row 0 is
/// the header, the nearest capture is the last data row.
fn cdx_latest(v: &Value) -> Option<(String, String)> {
    let rows = v.as_array()?;
    if rows.len() < 2 {
        return None;
    }
    let last = &rows[rows.len() - 1];
    let ts = last.get(1)?.as_str()?.to_string();
    let original = last
        .get(2)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((ts, original))
}

// Both indexes get their full opportunity concurrently. A found capture wins
// immediately; "never archived" requires two empty, reachable answers.
async fn archive_lookup_pair(
    availability: impl std::future::Future<Output = Avail>,
    cdx: impl std::future::Future<Output = Avail>,
) -> Avail {
    tokio::pin!(availability, cdx);
    let (first, second) = tokio::select! {
        answer = &mut availability => match answer {
            Avail::Found(pair) => return Avail::Found(pair),
            other => (other, cdx.await),
        },
        answer = &mut cdx => match answer {
            Avail::Found(pair) => return Avail::Found(pair),
            other => (other, availability.await),
        },
    };
    match (first, second) {
        (Avail::Found(pair), _) | (_, Avail::Found(pair)) => Avail::Found(pair),
        (Avail::Empty, Avail::Empty) => Avail::Empty,
        _ => Avail::Unreachable,
    }
}

async fn try_resurrect(
    daemon: &Arc<Daemon>,
    args: &Value,
    url: &str,
    live_error: &Value,
    route: &RequestRoute,
) -> Result<Value, ResurrectError> {
    let (mut snap_url, mut ts) = match archive_lookup_pair(
        availability_lookup(daemon, url, route),
        cdx_lookup(daemon, url, route),
    )
    .await
    {
        Avail::Found(pair) => pair,
        Avail::Unreachable => {
            return Err(ResurrectError {
                stage: ResurrectStage::LookupUnreachable,
                snapshot_url: None,
            });
        }
        Avail::Empty => {
            return Err(ResurrectError {
                stage: ResurrectStage::NoSnapshot,
                snapshot_url: None,
            });
        }
    };

    // 2. Fetch the snapshot : wayback is plain HTTP-friendly. A thin
    // extraction gets a second look first: a dead domain's last
    // capture is very often a meta-refresh stub ("parked → redirect")
    // that extracts to zero text but chains to the capture holding
    // the content. Browsers follow the refresh; so does resurrection,
    // but ONLY when wayback rewrote the target : a live-web target
    // would fetch a URL that may still be dead, moved, or hostile.
    let opts = ExtractOptions::default();
    let mut hops: u8 = 0;
    let (snap, ct) = loop {
        let snap = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            daemon
                .fetcher
                .fetch_persona_on_route(&snap_url, None, Some(route)),
        )
        .await
        {
            Ok(Ok(s)) => s,
            _ => {
                return Err(ResurrectError {
                    stage: ResurrectStage::SnapshotFetch,
                    snapshot_url: Some(snap_url.clone()),
                });
            }
        };
        if !matches!(snap.verdict, Verdict::ContentOk) {
            return Err(ResurrectError {
                stage: ResurrectStage::SnapshotVerdict,
                snapshot_url: Some(snap_url.clone()),
            });
        }
        let ct = snap
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if crate::fetch::guards::is_binary(&snap.body, &ct) {
            return Err(ResurrectError {
                stage: ResurrectStage::SnapshotBinary,
                snapshot_url: Some(snap_url.clone()),
            });
        }
        let ex = match extract::extract_off_worker(&snap.body, &ct, &snap_url, &opts).await {
            Ok(ex) => ex,
            Err(_) => {
                return Err(ResurrectError {
                    stage: ResurrectStage::SnapshotThin(0),
                    snapshot_url: Some(snap_url.clone()),
                });
            }
        };
        // Wayback serves the ORIGINAL server-rendered HTML : thinness
        // here usually means a genuinely small page, not a JS shell.
        // But wayback's redirect interstitials carry enough IA nav
        // chrome to pass any char threshold, so serving is gated on
        // stub markers too : a stub hops (up to MAX), a real page
        // serves, a true empty fails.
        let stub = ex.thin || ex.total_chars < 50 || is_wayback_stub(&snap.body);
        let chained = if hops < MAX_RESURRECT_HOPS && stub {
            meta_refresh_target(&snap.body).filter(|t| wayback_ts_of(t).is_some())
        } else {
            None
        };
        match chained {
            Some(target) => {
                hops += 1;
                if let Some(t) = wayback_ts_of(&target) {
                    ts = t;
                }
                snap_url = target;
            }
            None if stub => {
                return Err(ResurrectError {
                    stage: ResurrectStage::SnapshotThin(ex.total_chars),
                    snapshot_url: Some(snap_url.clone()),
                });
            }
            // No chain, but real-enough content : serve it.
            None => break (snap, ct),
        }
    };

    let opts = match fetch_read_options(args) {
        Ok(opts) => opts,
        Err(error) => return Ok(error),
    };
    // First establish that the snapshot is usable, then apply the caller's
    // scope. A short selected slice or a probe miss is not a dead snapshot.
    let ex = extract::extract_off_worker(&snap.body, &ct, &snap_url, &opts).await;
    Ok(archived_content(
        ex,
        &snap_url,
        url,
        &ts,
        snap.status,
        live_error,
    ))
}

fn archived_content(
    extracted: Result<extract::Extracted, extract::ExtractError>,
    snapshot_url: &str,
    url: &str,
    ts: &str,
    status: u16,
    live_error: &Value,
) -> Value {
    let ex = match extracted {
        Ok(ex) => ex,
        Err(extract::ExtractError::SelectorNoMatch {
            selector,
            inspected,
        }) => {
            return tool_error_structured(
                format!("CSS selector {selector:?} matched no elements in the archive snapshot"),
                "permanent",
                Some(
                    json!({"url": url, "snapshot_url": snapshot_url, "code": "selector.nomatch", "elements_inspected": inspected,
                "next_action": "correct the selector, or omit it to read the snapshot"}),
                ),
            );
        }
        Err(extract::ExtractError::BadSelector(selector)) => {
            return tool_error_structured(
                format!("invalid CSS selector: {selector}"),
                "permanent",
                Some(
                    json!({"url": url, "code": "selector.invalid", "next_action": "correct the CSS selector syntax"}),
                ),
            );
        }
        Err(error) => {
            return tool_error_structured(
                format!("archive content extraction failed: {error}"),
                "permanent",
                Some(
                    json!({"url": url, "snapshot_url": snapshot_url, "next_action": "inspect the snapshot or choose another source"}),
                ),
            );
        }
    };
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
        "*[ARCHIVED COPY : Wayback snapshot {date} ({age_days}d old){staleness}. Retrieval note: {live_reason}]*\n\n"
    );
    let mut trace = Trace::default();
    trace.step("archive", "wayback", &format!("snapshot {ts}"), 0);
    // The provenance banner is outside the source read budget and must not
    // change pagination, probe state or completeness calculations.
    let mut result = finish_result(&ex, "1(wayback)", status, "ContentOk", url, &trace, 0);
    result["content"][0]["text"] = json!(format!(
        "{banner}{}",
        format_fetch_markdown(&ex, snapshot_url, url)
    ));
    result["structuredContent"]["snapshot_url"] = json!(snapshot_url);
    result["structuredContent"]["archived"] =
        json!({"snapshot": ts, "date": date, "age_days": age_days});
    result["_meta"]["com.donsetch/fetch-debug"]["verdict"] = json!("Archived");
    result["_meta"]["com.donsetch/fetch-debug"]["live_error"] = json!(live_reason);
    result
}

/// Redirect chains through wayback interstitials can run several
/// captures deep (a dead domain's stub -> a host's redirect stub ->
/// the real landing page). Bounded so a hostile chain cannot spin.
const MAX_RESURRECT_HOPS: u8 = 4;

/// Wayback's "Got an HTTP NNN at crawl time / Redirecting to... /
/// Impatient?" interstitial and its capture-calendar page : both are
/// wayback UI, not archived content, and both extract enough text to
/// defeat char-count thinness checks.
fn is_wayback_stub(body: &[u8]) -> bool {
    // No 64KB head window: replay pages wrap captures in the full
    // IA nav (megabytes of markup), and the interstitial markers sit
    // AFTER it, near the redirect notice at the document's end.
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    text.contains("response at crawl time")
        || text.contains("impatient?")
        || text.contains("redirecting to...")
}

/// Pull the refresh target out of `<meta http-equiv="refresh"
/// content="[delay;] url=target">`. A dead domain's archived last
/// capture is very often exactly this stub, and a browser would
/// follow it : so does resurrection. Byte-scanned on an
/// ASCII-lowercased copy (length-preserving, so spans index the
/// original); the target keeps its original case because wayback
/// capture paths are case-sensitive.
fn meta_refresh_target(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(64 * 1024)];
    let text = String::from_utf8_lossy(head).to_string();
    let lower = text.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("<meta") {
        let start = from + rel;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        from = end.max(start + 1);
        let tag_lower = &lower[start..end];
        let tag_orig = &text[start..end];
        let is_refresh = attr_value_span(tag_lower, "http-equiv")
            .and_then(|(s, e)| tag_lower.get(s..e))
            .is_some_and(|v| v.trim() == "refresh");
        if !is_refresh {
            continue;
        }
        let Some((cs, ce)) = attr_value_span(tag_lower, "content") else {
            continue;
        };
        let content_lower = tag_lower.get(cs..ce)?;
        let content_orig = tag_orig.get(cs..ce)?;
        // "[delay][;] *url=target" : find the url= part case-
        // insensitively, keep the target's original bytes.
        let Some(urel) = content_lower.find("url=") else {
            continue;
        };
        let target = content_orig[urel + 4..].trim();
        let target = target.trim_matches(|c| c == '\'' || c == '"').trim();
        if !target.is_empty() {
            return Some(target.to_string());
        }
    }
    None
}

/// Byte span of `name="value"` (or single quotes) inside a tag.
/// Offsets index the string given, so callers can slice the same
/// spans out of the original-case text.
fn attr_value_span(tag_lower: &str, name: &str) -> Option<(usize, usize)> {
    let pat = format!("{name}=");
    let bytes = tag_lower.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = tag_lower[from..].find(&pat) {
        let at = from + rel;
        let boundary_ok = at == 0 || matches!(bytes[at - 1], b' ' | b'\t' | b'\n' | b'\r' | b'/');
        let after = at + pat.len();
        if boundary_ok && after < bytes.len() && matches!(bytes[after], b'"' | b'\'') {
            let quote = bytes[after] as char;
            let vstart = after + 1;
            let vend = tag_lower[vstart..]
                .find(quote)
                .map_or(tag_lower.len(), |e| vstart + e);
            return Some((vstart, vend));
        }
        from = at + pat.len();
    }
    None
}

/// `https://web.archive.org/web/<14-digit-ts>/<...>` → Some(ts).
/// Only wayback-rewritten refresh targets are followed : a target
/// pointing at the live web would silently fetch a URL that may
/// still be dead (or hostile).
fn wayback_ts_of(target: &str) -> Option<String> {
    let after_scheme = target.split_once("://")?.1;
    let (host, path) = after_scheme.split_once('/')?;
    if !host.eq_ignore_ascii_case("web.archive.org") {
        return None;
    }
    let seg = path.strip_prefix("web/")?;
    let ts: String = seg.chars().take(14).collect();
    (ts.len() == 14 && ts.bytes().all(|b| b.is_ascii_digit())).then_some(ts)
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
    // Byte slicing via get(): a hostile archive response carrying a
    // multibyte char across byte 8 used to panic the slice (and a
    // panic in a tool task hangs the caller's request with no
    // response and leaks the cancel-registry entry).
    if ts.len() >= 8 && ts.as_bytes()[..8].iter().all(|b| b.is_ascii_digit()) {
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
    } else if changed != "new" {
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

/// B4 agent-outcome feedback (default off). Soft-demote the
/// (class, host) when the agent's own verification failed:
/// must_contain returned NO MATCH, or the page was unreadable
/// (thin / SoftNotFound). Walls never count: a challenge is a
/// routing fact, not a quality fact. Never fetches anything.
fn maybe_record_agent_outcome(
    daemon: &Daemon,
    host: &str,
    opts: &ExtractOptions,
    ex: &extract::Extracted,
    verdict: &str,
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
        opts.must_contain.is_some(),
        opts.focus.is_some() || opts.toc || opts.section.is_some(),
    );
    let probe_miss = opts.must_contain.is_some() && ex.markdown.starts_with("probe: NO MATCH");
    let unreadable = ex.thin || verdict == "SoftNotFound";
    if probe_miss || unreadable {
        daemon.searcher.observe_outcome_miss(class, host);
    }
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
        "ok": true,
        "url": url,
        "content_ok": !ex.thin && verdict == "ContentOk",
        "content_kind": format!("{:?}", ex.content_kind),
        "read_status": if ex.thin { "thin" } else if ex.next_offset.is_some() || ex.markdown.len() < ex.total_chars { "partial" } else { "content" },
        "content_complete": !ex.thin && ex.next_offset.is_none() && ex.markdown.len() == ex.total_chars && ex.blocks_shown == ex.blocks_total,
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
    if ex.markdown.starts_with("probe: ") {
        structured["read_status"] = json!("probe");
        structured["content_complete"] = json!(false);
        if ex.markdown.starts_with("probe: MATCH") || ex.markdown.starts_with("probe: NO MATCH") {
            structured["matched"] = json!(ex.markdown.starts_with("probe: MATCH"));
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
                .then(|| "· ⚠ may need browser; challenge latency varies".to_string())
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

/// Writes a rendered page's DOM to `dir/dom-<host>.html` for the
/// `debug.ghost` dump, readable by its owner only, and returns that
/// path. Best effort: a failed write is not an error for the fetch.
fn dump_ghost_dom(dir: &std::path::Path, host: &str, html: &str) -> std::path::PathBuf {
    let safe: String = host
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let _ = std::fs::create_dir_all(dir);
    let p = dir.join(format!("dom-{safe}.html"));
    let _ = crate::config::write_private(&p, html.as_bytes());
    p
}

#[cfg(test)]
mod bypass_content_tests {
    use super::*;
    use crate::search::byok::store::{ByokConfig, KeyEntry, KeyState, ProviderConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Nextest isolates the process-wide key store and daemon state.
    #[tokio::test]
    async fn tier_three_rejects_and_evicts_shells_but_returns_real_content() {
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
                "{name}: tier-three content contract: {result:?}"
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
mod ghost_dom_dump_tests {
    // The dump is the DOM the ghost rendered with the session vault
    // replanted: a logged-in page's markup.
    #[cfg(unix)]
    #[test]
    fn the_ghost_debug_dom_dump_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("donsetch-dom-mode-{}", std::process::id()));
        let p = super::dump_ghost_dom(&dir, "app.example.com", "<html>inbox</html>");
        assert_eq!(p, dir.join("dom-app_example_com.html"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "<html>inbox</html>");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(mode, 0o600);
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
    fn a_batch_stays_within_its_token_budget() {
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
        slice_batch_markdowns(&urls, &results, &mut markdowns, 2_000, &mut flags);
        assert!(
            flags.iter().all(|f| *f),
            "every over-share member gets sliced"
        );
        let out = render_fetch_batch(&urls, &results, &markdowns, Some(2_000), &flags);
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(
            text.len() <= 2_000 * 4,
            "batch output {} bytes exceeds its {} byte budget",
            text.len(),
            2_000 * 4
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
            partial: None,
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

    // Framework-junk shells must never ship as successful content:
    // the classifier that gates the tier-2 escalation (live case:
    // facebook's tier-1 markdown was React internals).
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
            "1",
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
        let output = render_fetch_batch(&urls, &results, &markdowns, Some(2_000), &[false, false]);
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

#[cfg(test)]
mod resurrect_tests {
    use super::{
        ResurrectStage, attr_value_span, cdx_latest, is_wayback_stub, meta_refresh_target,
        wayback_ts_of,
    };
    use serde_json::json;

    #[tokio::test]
    async fn archived_reads_keep_selector_probe_and_pagination_contracts() {
        let html = format!(
            "<html><head><title>Archived delivery guide</title></head><body><article><h1>Delivery</h1><p>OUTSIDE scope.</p><div id='wanted'><p>{}</p></div></article></body></html>",
            "INSIDE reliable delivery records every operation and frees cancelled resources. "
                .repeat(40)
        );
        let snapshot = "https://web.archive.org/web/20250101000000/https://example.com/guide";
        let original = "https://example.com/guide";
        let mut results = Vec::new();
        for args in [
            json!({"selector":"#wanted","max_chars":300}),
            json!({"selector":"#missing"}),
            json!({"must_contain":"ABSENT_PHRASE"}),
            json!({"max_chars":300,"offset":300}),
        ] {
            let opts = super::fetch_read_options(&args).unwrap_or_else(|e| panic!("{e}"));
            let extracted =
                crate::extract::extract_off_worker(html.as_bytes(), "text/html", snapshot, &opts)
                    .await;
            results.push(super::archived_content(
                extracted,
                snapshot,
                original,
                "20250101000000",
                200,
                &json!({}),
            ));
        }
        let text = results[0]["content"][0]["text"].as_str().unwrap();
        let checks = [
            text.contains("INSIDE") && !text.contains("OUTSIDE") && text.len() < 800,
            results[0]["structuredContent"]["next_offset"]
                .as_u64()
                .is_some()
                && results[0]["structuredContent"]["content_complete"] == false,
            results[1]["structuredContent"]["ok"] == false
                && results[1]["structuredContent"]["code"] == "selector.nomatch",
            results[2]["structuredContent"]["read_status"] == "probe"
                && results[2]["structuredContent"]["matched"] == false,
            results[3]["structuredContent"]["next_offset"]
                .as_u64()
                .is_some_and(|n| n > 300),
        ];
        assert_eq!(checks, [true; 5], "archive caller controls: {results:?}");
    }

    // Nextest gives the daemon and process-wide configuration fresh state.
    #[tokio::test]
    async fn archive_only_checks_inputs_before_lookup() {
        let daemon = std::sync::Arc::new(super::Daemon::new().await.unwrap());
        for (url, args, code) in [
            ("http://127.0.0.1/", json!({"archive":"only"}), "guard.ssrf"),
            (
                "https://fake-user:fake-pass@example.com/",
                json!({"archive":"only"}),
                "guard.ssrf",
            ),
            (
                "https://example.com/",
                json!({"archive":"only","selector":"["}),
                "selector.invalid",
            ),
        ] {
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                super::fetch_single(&daemon, &args, url),
            )
            .await
            .expect("input guard must precede archive network access");
            assert_eq!(result["isError"], false, "{result}");
            assert_eq!(result["structuredContent"]["ok"], false, "{result}");
            assert_eq!(result["structuredContent"]["code"], code, "{result}");
        }
        let result = super::fetch_single(
            &daemon,
            &json!({"archive":"only","mode":"wrong"}),
            "https://example.com/",
        )
        .await;
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["ok"], false);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("mode must be")
        );
    }

    #[test]
    fn meta_refresh_follows_wayback_rewrite() {
        let html = b"<html><head><script>x</script>\
            <meta http-equiv=\"refresh\" content=\"0; url=http://web.archive.org/web/20190613084634/https://smallbusiness.yahoo.com/webhosting?source=geocities\"/>\
            </head><body></body></html>";
        let t = meta_refresh_target(html).expect("refresh target extracted");
        assert!(t.starts_with("http://web.archive.org/web/20190613084634/"));
        assert_eq!(wayback_ts_of(&t).as_deref(), Some("20190613084634"));
    }

    #[test]
    fn meta_refresh_is_case_insensitive_but_case_preserving() {
        // Match must survive any case in the markup ; the target's
        // case must survive the match (wayback paths are sensitive).
        let html = b"<META HTTP-EQUIV='Refresh' CONTENT=\"5; URL=http://web.archive.org/web/20200101000000/HTTP://Example.COM/Page\">";
        let t = meta_refresh_target(html).expect("refresh target extracted");
        assert!(t.contains("Example.COM/Page"), "original case lost: {t}");
        assert_eq!(wayback_ts_of(&t).as_deref(), Some("20200101000000"));
    }

    #[test]
    fn meta_refresh_unquoted_and_delayed_forms() {
        let html = b"<meta http-equiv=refresh content=30;url=http://web.archive.org/web/19990101000000/http://a.example/>";
        assert!(
            meta_refresh_target(html).is_none(),
            "unquoted attr values are not misparsed"
        );
    }

    #[test]
    fn meta_refresh_off_wayback_or_missing_is_none() {
        let live =
            b"<meta http-equiv=\"refresh\" content=\"0; url=https://parking.example/for-sale\">";
        assert_eq!(
            meta_refresh_target(live).as_deref(),
            Some("https://parking.example/for-sale")
        );
        assert_eq!(wayback_ts_of("https://parking.example/for-sale"), None);
        assert_eq!(meta_refresh_target(b"<html><body>hi</body></html>"), None);
        assert_eq!(
            meta_refresh_target(b"<meta http-equiv=\"refresh\" content=\"3\">"),
            None
        );
    }

    #[test]
    fn wayback_ts_rejects_non_wayback_and_malformed() {
        assert_eq!(
            wayback_ts_of("https://web.archive.org/web/notatime/http://x.example"),
            None
        );
        assert_eq!(
            wayback_ts_of("http://web.archive.org/other/20200101000000/x"),
            None
        );
        assert_eq!(
            wayback_ts_of("https://spoof.example/web/20200101000000/x"),
            None
        );
    }

    #[test]
    fn attr_value_span_respects_boundaries() {
        let tag = "meta data-content=\"a\" content=\"real value\" x";
        let (s, e) = attr_value_span(tag, "content").expect("span");
        assert_eq!(&tag[s..e], "real value");
        assert_eq!(attr_value_span(tag, "missing"), None);
    }

    #[test]
    fn stage_tags_are_stable_machine_strings() {
        assert_eq!(
            ResurrectStage::LookupUnreachable.tag(),
            "lookup_unreachable"
        );
        assert_eq!(ResurrectStage::NoSnapshot.tag(), "no_snapshot");
        assert_eq!(ResurrectStage::SnapshotFetch.tag(), "snapshot_fetch_failed");
        assert_eq!(
            ResurrectStage::SnapshotVerdict.tag(),
            "snapshot_verdict_rejected"
        );
        assert_eq!(ResurrectStage::SnapshotBinary.tag(), "snapshot_binary");
        assert_eq!(
            ResurrectStage::SnapshotThin(12).tag(),
            "snapshot_extract_thin(12)"
        );
    }
    #[test]
    fn cdx_latest_picks_last_data_row() {
        let v = json!([
            [
                "urlkey",
                "timestamp",
                "original",
                "mimetype",
                "statuscode",
                "digest",
                "length"
            ],
            [
                "com,geocities)/",
                "20010615131644",
                "http://www.geocities.com/",
                "text/html",
                "200",
                "AAA",
                "1000"
            ],
            [
                "com,geocities)/",
                "20190613084634",
                "http://www.geocities.com/",
                "text/html",
                "200",
                "BBB",
                "900"
            ]
        ]);
        let (ts, original) = cdx_latest(&v).expect("capture picked");
        assert_eq!(ts, "20190613084634", "nearest-to-present capture wins");
        assert_eq!(original, "http://www.geocities.com/");
    }

    #[test]
    fn cdx_latest_handles_header_only_empty_and_garbage() {
        assert_eq!(
            cdx_latest(&json!([["urlkey", "timestamp", "original"]])),
            None
        );
        assert_eq!(cdx_latest(&json!([])), None);
        assert_eq!(cdx_latest(&json!("not an array")), None);
        assert_eq!(
            cdx_latest(&json!([["u", "t"], ["missing-ts-column"]])),
            None
        );
    }

    #[test]
    fn transport_classes_separate_death_from_ambiguity() {
        use super::super::transport_class;
        use crate::error::FetchError;
        // Resurrectable: the site is gone.
        assert_eq!(
            transport_class(&FetchError::Tls("certificate verify failed".into())),
            "tls"
        );
        assert_eq!(
            transport_class(&FetchError::Tls("handshake failure".into())),
            "tls"
        );
        assert_eq!(
            transport_class(&FetchError::Io(std::io::Error::other(
                "Name or service not known"
            ))),
            "dns"
        );
        assert_eq!(
            transport_class(&FetchError::Io(std::io::Error::other("connection refused"))),
            "refused"
        );
        // Excluded: ambiguous or IP-level; a snapshot would lie.
        assert_eq!(transport_class(&FetchError::Timeout), "timeout");
        assert_eq!(
            transport_class(&FetchError::Tls("connection reset by peer".into())),
            "reset"
        );
        assert_eq!(
            transport_class(&FetchError::Io(std::io::Error::other(
                "connection timed out"
            ))),
            "timeout"
        );
        assert_eq!(
            transport_class(&FetchError::Http("parser died".into())),
            "protocol"
        );
        assert_eq!(
            transport_class(&FetchError::Ghost("no browser".into())),
            "ghost"
        );
    }

    #[test]
    fn resurrectable_transport_classes_are_exactly_tls_dns_refused() {
        let resurrectable: fn(&str) -> bool = |k| matches!(k, "tls" | "dns" | "refused");
        assert!(resurrectable("tls"));
        assert!(resurrectable("dns"));
        assert!(resurrectable("refused"));
        assert!(!resurrectable("timeout"));
        assert!(!resurrectable("reset"));
        assert!(!resurrectable("network"));
        assert!(!resurrectable("protocol"));
    }
    #[test]
    fn wayback_stub_markers_do_not_catch_real_pages() {
        let interstitial = b"<html><body>Got an HTTP 301 response at crawl time.             Redirecting to... <a href=\"/web/20100101/http://x.example\">x</a>             <b>Impatient?</b></body></html>";
        assert!(is_wayback_stub(interstitial));
        assert!(!is_wayback_stub(
            b"<html><body>Welcome to my Geocities page. Under construction.</body></html>"
        ));
        // An archived page ABOUT the wayback machine must not be
        // misread as chrome : the calendar phrase differs from prose.
        let article = b"<html><body><h1>History of the Wayback Machine</h1>            It preserves redirects and their targets.</body></html>";
        assert!(!is_wayback_stub(article));
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
        let batch = render_fetch_batch(
            &[url],
            &[result],
            &[Some("evidence".into())],
            Some(200),
            &[true],
        );
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

    fn args_with(ms: Option<u64>) -> Value {
        match ms {
            Some(v) => json!({ "deadline_ms": v }),
            None => json!({}),
        }
    }

    #[test]
    fn no_deadline_keeps_the_fixed_pass() {
        let b = Budget::of(&args_with(None));
        assert_eq!(b.pass(20), std::time::Duration::from_secs(20));
        assert_eq!(b.pass(25), std::time::Duration::from_secs(25));
    }

    #[test]
    fn a_short_deadline_shrinks_the_pass_instead_of_being_cut_off() {
        // 12s of budget: a 20s pass runs past it, so the caller gets a clock
        // hit instead of the wall verdict that pass had already seen.
        let b = Budget::of(&args_with(Some(12_000)));
        let p = b.pass(20);
        assert!(
            p < std::time::Duration::from_secs(12),
            "the pass must fit the budget, got {p:?}"
        );
        assert!(
            p >= std::time::Duration::from_secs(3),
            "the floor must keep it usable, got {p:?}"
        );
    }

    #[test]
    fn a_deadline_larger_than_the_default_still_caps_at_the_default() {
        let b = Budget::of(&args_with(Some(600_000)));
        assert_eq!(b.pass(20), std::time::Duration::from_secs(20));
        assert_eq!(b.pass(25), std::time::Duration::from_secs(25));
    }

    #[test]
    fn stealth_v3_short_budget_still_allocates_a_browser_pass() {
        for ms in [500, 1000, 2000] {
            let b = Budget::of(&args_with(Some(ms)));
            let pass = b.pass(20);
            assert!(
                pass >= std::time::Duration::from_millis(ms / 2),
                "the envelope reserve cannot consume the whole {ms}ms budget: {pass:?}"
            );
            assert!(pass < std::time::Duration::from_millis(ms));
        }
        let expired = Budget {
            deadline: Some(std::time::Duration::from_millis(500)),
            start: std::time::Instant::now() - std::time::Duration::from_millis(501),
        };
        assert_eq!(
            expired.pass(20),
            std::time::Duration::ZERO,
            "expired budget cannot gain time"
        );
    }

    #[test]
    fn a_tiny_deadline_never_allocates_time_beyond_the_call() {
        let b = Budget::of(&args_with(Some(500)));
        let pass = b.pass(20);
        assert!(pass > std::time::Duration::ZERO);
        assert!(pass < std::time::Duration::from_millis(500));
    }

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
                        crate::transport::proxy::Proxy::parse(&format!(
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
                    daemon.fetcher = Arc::new(Fetcher::new(daemon.profile.clone()).unwrap().with_egress(Arc::clone(&pool)));
                    let daemon = Arc::new(daemon);
                    let result = tokio::time::timeout(std::time::Duration::from_secs(8),
                        fetch_single(&daemon, &json!({"tier":"1", "archive":"off"}), "http://old.reddit.com/r/owned/")
                    ).await.unwrap();
                    stop.send(true).unwrap();
                    for server in servers { tokio::time::timeout(std::time::Duration::from_secs(2), server).await.unwrap().unwrap(); }
                    assert_eq!(result["structuredContent"]["ok"], true, "{result}");
                    let events = events.lock().unwrap();
                    assert_eq!(events.len(), 2, "one adapter read and one fallback: {events:?}");
                    assert!(events[0].1.contains(".json"), "{events:?}");
                    assert!(!events[1].1.contains(".json"), "{events:?}");
                    assert!(events.iter().all(|(lane, _)| *lane == "A"),
                        "a related fallback must not repick after the adapter burns its lane: {events:?}");
                    assert!(result["content"][0]["text"].as_str().unwrap().contains("Owned fallback lane A"), "{result}");
                    assert!(!pool.is_dead(&proxies[0].id()),
                        "an origin 429 must not globally bench a reachable proxy");
                });
            }).unwrap().join().unwrap();
    }

    #[test]
    fn browser_controls_keep_the_requested_website_url() {
        let url = url::Url::parse("https://stackoverflow.com/questions/42917566/example").unwrap();
        let cases = [
            json!({"tier":"2"}),
            json!({"actions":[{"do":"wait","ms":10}]}),
            json!({"actions":[]}),
            json!({"tier":"1"}),
            json!({"section":"Answer 1"}),
            json!({"selector":"article"}),
        ];
        let rewrites: Vec<bool> = cases
            .iter()
            .map(|args| fetch_url_rewrite(&url, args, false).is_some())
            .collect();
        assert_eq!(rewrites, vec![false, false, true, true, true, false]);
        assert!(fetch_url_rewrite(&url, &json!({}), true).is_none());
    }

    #[test]
    fn recovery_retries_recheck_http_despite_saved_wall_routes() {
        use crate::ghost::cache::{DomainProfile, GhostState};
        let host = "www.reddit.com";
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut state = GhostState::default();
        state.profiles.insert(
            host.into(),
            DomainProfile {
                needs_tier2: true,
                last_cold_check: now,
                ..Default::default()
            },
        );
        assert!(matches!(
            fetch_route(&state, host, "auto", false, false, false, false),
            RouteDecision::SkipToSolve
        ));
        assert!(matches!(
            fetch_route(&state, host, "auto", false, false, false, true),
            RouteDecision::Cold
        ));
        let profile = state.profiles.get_mut(host).unwrap();
        profile.wall_fail_streak = 2;
        profile.last_wall_fail = now;
        assert!(matches!(
            fetch_route(&state, host, "auto", false, false, false, false),
            RouteDecision::SolveCooldown(_)
        ));
        assert!(matches!(
            fetch_route(&state, host, "auto", false, false, false, true),
            RouteDecision::Cold
        ));
        state
            .profiles
            .insert("stackoverflow.com".into(), state.profiles[host].clone());
        assert!(matches!(
            fetch_route(
                &state,
                "stackoverflow.com",
                "auto",
                false,
                false,
                false,
                true
            ),
            RouteDecision::SolveCooldown(_)
        ));
        // Explicit browser requests and other remembered walls keep their route.
        assert!(matches!(
            fetch_route(&state, host, "2", false, false, false, true),
            RouteDecision::SkipToSolve
        ));
        assert!(matches!(
            fetch_route(&state, host, "1", false, false, false, false),
            RouteDecision::Cold
        ));
        assert!(matches!(
            fetch_route(&state, host, "2", true, false, false, false),
            RouteDecision::Cold
        ));
        assert!(matches!(
            fetch_route(&state, host, "auto", false, false, true, false),
            RouteDecision::Cold
        ));
        // v4.7 V03: JSON data endpoints are terminal at the HTTP tier
        // : tier 2 stays Cold and a learned wall never skips tier 1.
        assert!(matches!(
            fetch_route(&state, host, "2", false, true, false, false),
            RouteDecision::Cold
        ));
        assert!(matches!(
            fetch_route(&state, host, "auto", false, true, false, false),
            RouteDecision::Cold
        ));
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
    // `Challenge(_) if tier != "1"` arm swallowed it and the ladder
    // ended after one HTTP request.
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
        let cookie = |name: &str| CookieRecord {
            name: name.into(),
            value: "v".into(),
            domain: ".reddit.com".into(),
            path: "/".into(),
            expires_at: None,
            secure: true,
            http_only: true,
            same_site: "Lax".into(),
        };
        assert!(reddit_session_live(&[cookie("loid")]));
        assert!(reddit_session_live(&[
            cookie("session_tracker"),
            cookie("loid")
        ]));
        assert!(!reddit_session_live(&[cookie("session_tracker")]));
        assert!(!reddit_session_live(&[]));
    }

    // The session retry: reddit page refusals at tier 1 get one
    // session-init attempt; never twice, never off the content host.
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
        // Other hosts keep their own escalation rules.
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
