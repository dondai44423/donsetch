//! The structured error contract for the MCP tool surface:
//! stable `error_code` mapping, `next_action` guidance, the
//! escalation `Trace`, and the human-facing message builders.
//! Every tool error flows through here so codes and actions
//! stay consistent across fetch/search/crawl.

use serde_json::{Value, json};

use super::*;
pub(super) fn friendly_fetch_error(e: &FetchError) -> String {
    match e {
        FetchError::Timeout => "request timed out (the server took too long to respond)".into(),
        FetchError::TooManyRedirects => "too many redirects (the URL loops)".into(),
        FetchError::InvalidUrl(u) => format!("invalid URL: {u}"),
        FetchError::Tls(msg) => {
            // Classified errors (egress interception, cert trust) carry
            // the actionable hint the user needs to fix their network;
            // pass those through verbatim. Raw SSL/BoringSSL internals
            // get flattened into short honest messages.
            if msg.contains("egress path")
                || msg.contains("certificate verification failed")
                || msg.contains("TLS handshake aborted")
                || msg.contains("TLS handshake cut short")
            {
                format!("TLS error: {msg}")
            } else {
                let msg = msg.to_lowercase();
                if msg.contains("certificate") || msg.contains("handshake") {
                    "TLS error: the server's certificate or handshake failed".into()
                } else if msg.contains("reset") || msg.contains("eof") {
                    "connection reset by server".into()
                } else {
                    "TLS connection failed".into()
                }
            }
        }
        FetchError::Io(e) => {
            let msg = e.to_string();
            if msg.contains("refused") {
                "connection refused (the server is not accepting connections)".into()
            } else if msg.contains("timed out") {
                "connection timed out".into()
            } else if msg.contains("not found") || msg.contains("no address") {
                "host not found (DNS lookup failed)".into()
            } else if msg.contains("reset") {
                "connection reset by server".into()
            } else {
                format!("network error: {e}")
            }
        }
        FetchError::Http(msg) => {
            // h1/h2 protocol errors: strip raw parser messages.
            let msg = msg.to_lowercase();
            if msg.contains("eof before headers") {
                "server closed the connection before sending a response".into()
            } else if msg.contains("read_server_hello") {
                "TLS handshake failed (server rejected the connection)".into()
            } else {
                format!("HTTP protocol error: {e}")
            }
        }
        FetchError::Ghost(msg) => format!("browser automation error: {msg}"),
    }
}

/// Map a Verdict + status code to a clean, specific error message.
/// Distinguishes genuine blocks from upstream errors from SPAs.
pub(super) fn verdict_error(verdict: Verdict, status: u16, url: &str) -> String {
    match verdict {
        Verdict::AuthWall => {
            format!("HTTP 401 at {url} : the server requires authentication")
        }
        Verdict::Paywall => format!("paywall: {url} requires payment to view content"),
        Verdict::SoftNotFound => format!("not found: {url} returned HTTP {status}"),
        Verdict::Blocked => {
            // 403/429 without challenge markers = upstream block, not a bot wall.
            match status {
                403 => format!("forbidden: {url} returned HTTP 403 (access denied)"),
                429 => format!("rate limited: {url} returned HTTP 429 (too many requests)"),
                503 => format!(
                    "service unavailable: {url} returned HTTP 503 (server overloaded or down)"
                ),
                _ => format!("blocked: {url} returned HTTP {status}"),
            }
        }
        Verdict::Challenge(v) => format!(
            "bot wall: {url} is protected by {:?} (try fetch with tier=2 for headless browser)",
            v
        ),
        Verdict::ContentOk => format!("unexpected error: {url} (status {status})"),
    }
}

/// The fetch tool: tier 1 → verdict → ghost solve/render
/// → DonSift. Ports the CLI escalation into the daemon,
/// with warm-start and render cache.
#[allow(clippy::field_reassign_with_default)]
pub(super) fn deadline_error(url: &str) -> Value {
    let mut trace = Trace::default();
    trace.step("clock", "deadline", "hit", 0);
    tool_error_structured(
        format!("fetch: deadline_ms exceeded at {url}"),
        "transient",
        Some(json!({
            "url": url,
            "escalation": trace.value(),
            "next_action": "retry with a higher deadline_ms, or tier=1 (skips browser escalation : the usual deadline eater on walled sites)",
        })),
    )
}

/// Resolve a raw url-or-handle argument to a fetchable http(s)
/// URL. Ok(URL) or Err(error Value).
pub(super) fn search_deadline_error(query: &str) -> Value {
    let mut trace = Trace::default();
    trace.step("search", "engines", "deadline", 0);
    tool_error_structured(
        format!("search: deadline_ms exceeded for \"{query}\""),
        "transient",
        Some(json!({
            "query": query,
            "escalation": trace.value(),
            "next_action": "retry with a higher deadline_ms, or without one (engines have their own timeouts)",
        })),
    )
}

pub(super) fn search_batch_deadline_error(queries: &[String]) -> Value {
    let mut trace = Trace::default();
    trace.step("search", "query-variants", "deadline", 0);
    tool_error_structured(
        format!(
            "search: deadline_ms exceeded while running {} query variants",
            queries.len()
        ),
        "transient",
        Some(json!({
            "queries": queries,
            "escalation": trace.value(),
            "next_action": "retry with a higher deadline_ms, fewer query_variants, or a single query",
        })),
    )
}

/// Ghost render capability shared by the crawl and the search
/// SERP cascade lane. `skip_cache_read` = never serve a previous
/// render from the cache (the search lane uses this: a cached
/// walled SERP would replay "no results" for the whole TTL).
/// Writes are always kept: the cache still serves normal fetches
/// of the same URL.
pub(super) fn search_error(query: &str, cause: &str, byok_tried: bool, kind: &str) -> Value {
    if kind == "permanent" {
        // validate_query rejected the query before any engine or
        // provider was ever contacted : no escalation trace to show,
        // and retrying the same query (or adding an API key) won't
        // help, unlike the exhausted-engines case below.
        return tool_error_structured(
            format!("search: {cause}"),
            "permanent",
            Some(json!({
                "query": query,
                "next_action": "fix the query and search again",
            })),
        );
    }
    let mut trace = Trace::default();
    trace.step("search", "engines", "error", 0);
    if byok_tried {
        trace.step("byok", "providers", "error", 0);
    }
    let mut hint = String::from(
        "all engines failed : transient in most cases: retry once, then simplify the query",
    );
    if !byok_tried {
        hint.push_str(
            "; if repeated, add an API key provider (donsetch keys add) for a fallback path",
        );
    }
    tool_error_structured(
        format!("search: {cause}"),
        "transient",
        Some(json!({
            "query": query,
            "escalation": trace.value(),
            "next_action": hint,
        })),
    )
}

pub(super) fn tool_error(message: impl Into<String>) -> Value {
    tool_error_kind(message, "permanent")
}

/// Like `tool_error` but with an explicit `errorKind` for CLI
/// exit-code mapping. `kind` is one of: "permanent", "transient",
/// "walled". MCP clients ignore the extra field; the CLI uses it
/// to choose exit 1 / 2 / 3.
pub(super) fn tool_error_kind(message: impl Into<String>, kind: &str) -> Value {
    tool_error_structured(message, kind, None)
}

/// Error with structure: the 50-case report asked for honest
/// machine-readable failure state : status, verdict, url,
/// next_action, and the escalation trace : so an agent can
/// decide its fallback without parsing prose. Human message
/// stays in content[0].text exactly as before.
/// v3 error taxonomy: stable machine-readable codes so agents
/// branch on `code`, not prose. One classifier, every tool.
///
/// | code | meaning |
/// |---|---|
/// | network.dns / network.timeout / network.ratelimit | transport |
/// | wall.challenge / wall.captcha / wall.paywall / wall.auth | blocked |
/// | cloak.suspected | tier-1 content is likely decoy |
/// | content.notfound / content.binary / content.oversize / content.extract | body |
/// | guard.ssrf | blocked by design |
/// | parse.encoding | charset-level failure |
/// | archive.stale | served an old snapshot |
/// | deadline.hit | time budget exhausted |
/// | crawl.seed / crawl.resume / fetch.invalid | input errors |
pub(super) fn error_code(msg: &str, structured: Option<&Value>) -> &'static str {
    let m = msg.to_ascii_lowercase();
    let v = structured
        .and_then(|s| s.get("verdict"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match () {
        _ if m.contains("ssrf") || m.contains("private/loopback") => "guard.ssrf",
        _ if m.contains("deadline") => "deadline.hit",
        _ if m.contains("dns") => "network.dns",
        _ if m.contains("timeout") || m.contains("timed out") => "network.timeout",
        _ if m.contains("rate limit") || m.contains("429") => "network.ratelimit",
        _ if m.contains("binary content") => "content.binary",
        _ if m.contains("too large") || m.contains("oversize") => "content.oversize",
        _ if m.contains("invalid url") => "fetch.invalid",
        _ if m.contains("bad seed") => "crawl.seed",
        _ if m.contains("resume token") => "crawl.resume",
        _ if m.contains("charset") || m.contains("decode") => "parse.encoding",
        _ if m.contains("captcha") => "wall.captcha",
        _ if m.contains("archived copy") || m.contains("snapshot") => "archive.stale",
        _ if v == "Challenge" => "wall.challenge",
        _ if v == "Paywall" => "wall.paywall",
        _ if v == "AuthWall" => "wall.auth",
        _ if v == "SoftNotFound" => "content.notfound",
        _ if m.contains("tls error") && m.contains("certificate verification failed") => {
            "tls.verify"
        }
        _ if m.contains("tls handshake aborted") || m.contains("tls handshake cut short") => {
            "tls.egress"
        }
        _ if m.contains("extraction failed") || m.contains("no content") => "content.extract",
        _ if m.contains("cloak") => "cloak.suspected",
        _ => "content.extract",
    }
}

pub(super) fn tool_error_structured(
    message: impl Into<String>,
    kind: &str,
    structured: Option<Value>,
) -> Value {
    let mut text = message.into();
    // Fold next_action from structured into the text for clients
    // (Claude Code, VSCode) that drop text when structuredContent
    // is present. next_action is critical for agent recovery.
    if let Some(ref s) = structured
        && let Some(action) = s.get("next_action").and_then(Value::as_str)
        && !action.is_empty()
    {
        text.push_str(&format!("\n\nNext action: {action}"));
    }
    let code = error_code(&text, structured.as_ref());
    let mut v = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
        "errorKind": kind,
        "code": code
    });
    if let Some(mut s) = structured {
        // The stable code lives where agents read it.
        s["code"] = json!(code);
        v["structuredContent"] = s;
    }
    v
}

/// What should the agent DO next, given this failure? One line,
/// actionable, derived from verdict + kind. The report's core
/// ask: "make failures unambiguous."
pub(super) fn next_action_for(verdict: Option<Verdict>, status: u16, kind: &str) -> String {
    match verdict {
        Some(Verdict::AuthWall) => {
            "requires login credentials : no keyless automated path; use an interactive browser with your session".into()
        }
        Some(Verdict::Paywall) => {
            "paid content : no automated path; look for an open preprint/copy via web_search".into()
        }
        Some(Verdict::SoftNotFound) => {
            "verify the URL (typo? deleted page?) : or web_search the page title to find the moved copy".into()
        }
        Some(Verdict::Challenge(_)) if kind == "walled" => {
            "tier 2 browser could not solve it : interactive verification needed; no automated path (by design DonSeTch does not solve captchas)".into()
        }
        Some(Verdict::Challenge(_)) => {
            "retry with tier=2 (or tier=auto) : the headless browser solves most JS/cookie challenges".into()
        }
        Some(Verdict::Blocked) => match status {
            429 => "rate limited : wait 30-60s and retry".into(),
            403 => "access denied : retry later or from a different network; this server refuses bots".into(),
            _ => "server rejected the request : retrying later sometimes works".into(),
        },
        _ if kind == "transient" => {
            "transient network failure : safe to retry immediately".into()
        }
        _ if kind == "walled" => {
            "no extractable content behind the wall : use an interactive agent browser for this site".into()
        }
        _ if kind == "tls.verify" => {
            "the interception CA is not trusted: export SSL_CERT_FILE pointing at the network's CA bundle and retry (donsetch doctor reports both trust stores)".into()
        }
        _ if kind == "tls.egress" => {
            "the egress path is intercepting HTTPS: export HTTPS_PROXY/HTTP_PROXY to route fetches through the network proxy (env-proxy convention, DONSETCH_NO_ENV_PROXY to disable) and retry".into()
        }
        _ => "check the URL and retry; if repeated, the site may be down or blocking".into(),
    }
}

/// Escalation trace: the ordered record of what DonSeTch tried :
/// HTTP → browser → OCR-style fallbacks : with tier, action,
/// outcome and per-step latency. Successes expose it through client-only
/// `_meta`; errors retain actionable state on the model surface.
#[derive(Default)]
pub(super) struct Trace {
    steps: Vec<Value>,
}

impl Trace {
    pub(super) fn step(&mut self, tier: &str, action: &str, outcome: &str, ms: u128) {
        self.steps.push(json!({
            "tier": tier,
            "action": action,
            "outcome": outcome,
            "ms": ms,
        }));
    }

    pub(super) fn value(&self) -> Value {
        Value::Array(self.steps.clone())
    }
}

/// Classify a wall verdict into an errorKind for CLI exit codes.
pub(super) fn verdict_kind(v: Verdict, status: u16) -> &'static str {
    match v {
        Verdict::Challenge(_) | Verdict::AuthWall | Verdict::Paywall => "walled",
        Verdict::Blocked if status == 429 || status == 503 => "transient",
        _ => "permanent",
    }
}

/// Classify a network/fetch error into an errorKind.
/// Which failure class a batch-level error collapses to, when a
/// batch carries mixed outcomes. Sibling of the single error codes.
pub(super) fn batch_failure_kind<'a>(kinds: impl Iterator<Item = &'a str>) -> &'static str {
    if kinds.into_iter().all(|k| k == "permanent") {
        "permanent"
    } else {
        "transient"
    }
}

// Execute explicit query variants concurrently and keep every result set
// separate. Grouped evidence lets the calling model compare formulations
// while each query retains DonSeTch's established ranking semantics.

pub(super) fn fetch_error_kind(e: &FetchError) -> &'static str {
    match e {
        FetchError::Timeout | FetchError::Io(_) => "transient",
        // Match on the classifier's own leading sentences, never on
        // hint text : "SSL_CERT_FILE" appears in BOTH hints, and
        // with it in the verify arm (checked first) every egress
        // failure classified as tls.verify; the egress arm was dead.
        FetchError::Tls(msg)
            if msg.starts_with("TLS handshake aborted")
                || msg.starts_with("TLS handshake cut short") =>
        {
            "tls.egress"
        }
        FetchError::Tls(msg)
            if msg.starts_with("TLS certificate verification failed")
                || msg.contains("trusted root") =>
        {
            "tls.verify"
        }
        _ => "permanent",
    }
}
#[cfg(test)]
mod stitch_tests {
    use super::*;

    // fetch_error_kind's tls.verify arm matched on "SSL_CERT_FILE" :
    // text that also appears in the egress hint appended to every
    // aborted/cut-short handshake message, so ALL egress failures
    // classified as tls.verify and the tls.egress arm was dead code
    // (wrong next_action: "export SSL_CERT_FILE" instead of the
    // proxy-routing hint the interception fix exists to give).
    // Classify against the REAL classifier output, not hand-written
    // strings, so the two files cannot drift apart again.
    #[test]
    pub(super) fn tls_error_kinds_match_the_real_classifier_output() {
        #[derive(Debug)]
        struct E(&'static str);
        impl std::fmt::Display for E {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for E {}
        let classified = |raw: &'static str| {
            FetchError::Tls(crate::transport::tls::classify_handshake_error(&E(raw)))
        };
        // Middlebox kills the handshake: reset / early EOF.
        assert_eq!(
            fetch_error_kind(&classified("connection reset by peer")),
            "tls.egress"
        );
        assert_eq!(
            fetch_error_kind(&classified("unexpected EOF during handshake")),
            "tls.egress"
        );
        // Re-signed cert from an untrusted interception CA.
        assert_eq!(
            fetch_error_kind(&classified("certificate verify failed: unknown ca")),
            "tls.verify"
        );
        // Unclassified boring text stays permanent.
        assert_eq!(
            fetch_error_kind(&classified("some exotic library error")),
            "permanent"
        );
    }

    // search_error used to hardcode errorKind: "transient" for every
    // failure, including validate_query rejections (empty/oversized
    // query) that never contact an engine -- contradicting its own
    // caller's comment ("a bad query is a permanent-shaped failure")
    // and, via exit_code_of in cli/tool.rs, handing scripts the wrong
    // exit code for a non-retryable input error.
    #[test]
    pub(super) fn search_error_permanent_has_no_false_escalation_trace() {
        let v = search_error(
            "",
            "empty query : pass a non-empty query string",
            false,
            "permanent",
        );
        assert_eq!(v["errorKind"], "permanent");
        assert!(
            v["structuredContent"].get("escalation").is_none(),
            "a validation failure never contacted an engine: no escalation trace to show"
        );
    }

    #[test]
    pub(super) fn search_error_transient_keeps_engine_escalation_trace() {
        let v = search_error("q", "all engines timed out", false, "transient");
        assert_eq!(v["errorKind"], "transient");
        assert!(v["structuredContent"].get("escalation").is_some());
    }

    #[test]
    pub(super) fn batch_failure_kind_permanent_only_when_every_variant_is() {
        assert_eq!(
            batch_failure_kind(["permanent", "permanent"].into_iter()),
            "permanent"
        );
        assert_eq!(batch_failure_kind(["permanent"].into_iter()), "permanent");
    }

    #[test]
    pub(super) fn batch_failure_kind_transient_if_any_variant_is() {
        // One transient variant means a retry could still succeed :
        // the batch as a whole should be reported retryable.
        assert_eq!(
            batch_failure_kind(["permanent", "transient"].into_iter()),
            "transient"
        );
        assert_eq!(
            batch_failure_kind(["transient", "transient"].into_iter()),
            "transient"
        );
    }

    #[test]
    pub(super) fn rel_next_found_and_resolved() {
        let html = r#"<html><head>
            <link rel="prev" href="/p1">
            <link rel="next chapter" href="/p3?page=2">
        </head><body></body></html>"#;
        // "/p3" is root-absolute: joins against the origin.
        assert_eq!(
            find_rel_next(html, "https://example.com/story/p2"),
            Some("https://example.com/p3?page=2".to_string())
        );
    }

    #[test]
    pub(super) fn anchor_rel_next_works() {
        let html = r#"<a rel="next" href="page-3.html">Next</a>"#;
        assert_eq!(
            find_rel_next(html, "https://example.com/book/page-2.html"),
            Some("https://example.com/book/page-3.html".to_string())
        );
    }

    #[test]
    pub(super) fn no_next_is_none() {
        assert!(find_rel_next("<html></html>", "https://example.com/").is_none());
    }

    #[test]
    pub(super) fn part_frontmatter_stripped() {
        let part =
            "# My Story\nhttps://example.com/p2\n> Same description\n\nPart two content here.";
        assert_eq!(strip_part_frontmatter(part), "Part two content here.");
        assert_eq!(strip_part_frontmatter("Just content"), "Just content");
    }
}
#[cfg(test)]
mod error_code_tests {
    use super::error_code;
    use serde_json::json;

    #[test]
    pub(super) fn codes_are_stable() {
        assert_eq!(
            error_code(
                "blocked: 10.0.0.1 is a private/loopback address : SSRF guard",
                None
            ),
            "guard.ssrf"
        );
        assert_eq!(
            error_code("deadline: exceeded 2000ms", None),
            "deadline.hit"
        );
        assert_eq!(error_code("dns: resolve failed", None), "network.dns");
        assert_eq!(
            error_code("walled", Some(&json!({"verdict": "Challenge"}))),
            "wall.challenge"
        );
        assert_eq!(
            error_code("walled", Some(&json!({"verdict": "Paywall"}))),
            "wall.paywall"
        );
        assert_eq!(
            error_code("binary content: image/png", None),
            "content.binary"
        );
        assert_eq!(error_code("crawl: bad seed URL", None), "crawl.seed");
        assert_eq!(
            error_code("crawl: resume token expired", None),
            "crawl.resume"
        );
        assert_eq!(error_code("fetch: invalid URL", None), "fetch.invalid");
    }
}
