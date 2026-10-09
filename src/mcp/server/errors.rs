//! The structured error contract for the MCP tool surface:
//! stable `error_code` mapping, `next_action` guidance, the
//! escalation `Trace`, and the human-facing message builders.
//! Every tool error flows through here so codes and actions
//! stay consistent across fetch/search/crawl.

use std::borrow::Cow;

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
        FetchError::ProxyConfig(msg) => format!("proxy configuration error: {msg}"),
        FetchError::Ghost(msg) => format!("browser automation error: {msg}"),
        // A name failure is not a policy block: the DNS variants carry
        // the honest message, and the code the agent reads comes from
        // the variant, not from this prose (#248).
        FetchError::Dns(msg) => format!("host could not be resolved (DNS): {msg}"),
        FetchError::DnsTimeout(msg) => {
            format!("DNS lookup timed out: {msg} (transient, a retry may work)")
        }
        FetchError::Ssrf(msg) => format!("blocked: {msg}"),
    }
}

/// Map a Verdict + status code to a clean, specific error message.
/// Distinguishes genuine blocks from upstream errors from SPAs.
pub(super) fn verdict_error(verdict: Verdict, status: u16, url: &str) -> String {
    match verdict {
        Verdict::AuthWall => {
            format!("authentication required at {url} (HTTP {status})")
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

/// Message sniffing reads symptoms, never data : error text quotes
/// the URL we looked at, and the URL's own port (`:42945`), path
/// (`/dns/`), or query (`?timeout=5`) must not be read as the
/// failure. Strips every http(s) URL token before classification.
/// (V24: a Cloudflare wall on port 42945 classified as a rate limit.)
fn without_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let hit = match (rest.find("http://"), rest.find("https://")) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(i) = hit else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i]);
        out.push(' ');
        let url = &rest[i..];
        let end = url.find(|c: char| c.is_whitespace()).unwrap_or(url.len());
        rest = &url[end..];
    }
}

/// True when `needle` occurs in `text` not embedded in a longer run
/// of ASCII digits : a port (`:42945`) or a duration (`1429ms`) is
/// not an HTTP status.
fn has_standalone_digits(text: &str, needle: &str) -> bool {
    let digit = |c: Option<char>| c.is_some_and(|d| d.is_ascii_digit());
    text.match_indices(needle).any(|(i, _)| {
        !digit(text[..i].chars().next_back()) && !digit(text[i + needle.len()..].chars().next())
    })
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
/// | browser.transport / browser.timeout | browser automation |
/// | wall.challenge / wall.challenge_unsolved / wall.empty_shell / wall.captcha / wall.paywall / wall.auth | blocked |
/// | cloak.suspected | tier-1 content is likely decoy |
/// | content.notfound / content.binary / content.oversize / content.extract / content.incomplete | body |
/// | guard.ssrf | blocked by design |
/// | parse.encoding | charset-level failure |
/// | deadline.hit | time budget exhausted |
/// | crawl.seed / crawl.resume / fetch.invalid / search.invalid / crawl.invalid | input errors |
pub(super) fn error_code(msg: &str, structured: Option<&Value>) -> Cow<'static, str> {
    // A producer that knows its own failure type outranks the text
    // classifier. The guard KNOWS a name it could not resolve is a name
    // problem; recovering that from prose is how a typo, a dead domain or
    // a resolver hiccup came back as a policy block (#248).
    if let Some(code) = structured
        .and_then(|s| s.get("code"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
    {
        return Cow::Owned(code.to_string());
    }
    let m = without_urls(&msg.to_ascii_lowercase());
    let v = structured
        .and_then(|s| s.get("verdict"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Cow::Borrowed(match () {
        _ if m.contains("proxy configuration:") || m.starts_with("proxy configuration error:") => {
            "proxy.config"
        }
        _ if m.contains("ssrf")
            || m.contains("private/loopback")
            || m.contains("blocked by design") =>
        {
            "guard.ssrf"
        }
        _ if m.contains("deadline") => "deadline.hit",
        _ if m.contains("cdp timeout:") || m.contains("cdp connect: ws handshake timeout") => {
            "browser.timeout"
        }
        _ if m.contains("cdp link closed")
            || m.contains("cdp dropped:")
            || m.contains("cdp send:")
            || m.contains("cdp connect:") =>
        {
            "browser.transport"
        }
        _ if m.contains("dns") => "network.dns",
        _ if m.contains("timeout") || m.contains("timed out") => "network.timeout",
        _ if m.contains("rate limit") || has_standalone_digits(&m, "429") => "network.ratelimit",
        _ if m.contains("binary content") => "content.binary",
        _ if v == "Incomplete" || m.contains("browser document incomplete") => "content.incomplete",
        _ if m.contains("too large") || m.contains("oversize") => "content.oversize",
        _ if m.contains("invalid url") => "fetch.invalid",
        _ if m.contains("bad seed") => "crawl.seed",
        _ if m.contains("resume token") => "crawl.resume",
        _ if m.contains("charset") || m.contains("decode") => "parse.encoding",
        _ if m.contains("anti-bot challenge that did not clear") => "wall.challenge_unsolved",
        _ if m.contains("navigation and login chrome") => "wall.empty_shell",
        _ if m.contains("captcha") => "wall.captcha",
        _ if v.starts_with("Challenge") => "wall.challenge",
        _ if v == "Blocked" => "wall.blocked",
        _ if v == "Paywall" => "wall.paywall",
        _ if v == "AuthWall" || m.contains("authentication required") => "wall.auth",
        _ if v == "SoftNotFound" || m.starts_with("not found:") => "content.notfound",
        _ if m.contains("tls error") && m.contains("certificate verification failed") => {
            "tls.verify"
        }
        _ if m.contains("tls handshake aborted") || m.contains("tls handshake cut short") => {
            "tls.egress"
        }
        _ if m.contains("extraction failed") || m.contains("no content") => "content.extract",
        _ if m.contains("cloak") => "cloak.suspected",
        _ => "content.extract",
    })
}

pub(super) fn tool_error_structured(
    message: impl Into<String>,
    kind: &str,
    mut structured: Option<Value>,
) -> Value {
    let mut text = message.into();
    let code = error_code(&text, structured.as_ref());
    let kind = if matches!(code.as_ref(), "browser.transport" | "browser.timeout") {
        let state = structured.get_or_insert_with(|| json!({}));
        if state["retry_safe"] == false {
            state["next_action"] = json!(
                "inspect the result with a plain fetch without actions; earlier actions may have completed, so do not replay them automatically"
            );
            "permanent"
        } else {
            state["next_action"] = json!(
                "retry the read once; if it repeats, inspect browser diagnostics and available resources"
            );
            "transient"
        }
    } else if code == "proxy.config" {
        let state = structured.get_or_insert_with(|| json!({}));
        state["next_action"] = json!(
            "correct the selected proxy in config or the proxy environment variable, then retry; run donsetch doctor to inspect proxy configuration"
        );
        state["retry_safe"] = json!(false);
        "permanent"
    } else {
        kind
    };
    // Fold next_action from structured into the text for clients
    // (Claude Code, VSCode) that drop text when structuredContent
    // is present. next_action is critical for agent recovery.
    if let Some(ref s) = structured
        && let Some(action) = s.get("next_action").and_then(Value::as_str)
        && !action.is_empty()
    {
        text.push_str(&format!("\n\nNext action: {action}"));
    }
    let mut v = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
        "errorKind": kind,
        "code": code
    });
    {
        let mut s = structured.unwrap_or_else(|| json!({
            "next_action": if kind == "permanent" { "correct the request before retrying" } else { "retry once; if the failure repeats, inspect the reported cause" }
        }));
        if s.get("next_action")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            s["next_action"] = json!(if kind == "permanent" {
                "correct the request before retrying"
            } else {
                "retry once; if the failure repeats, inspect the reported cause"
            });
        }
        // The stable code lives where agents read it (inside
        // structuredContent so it survives the TextOnly [meta] fold,
        // which clones structuredContent). v4.7 uniform envelope: a
        // classified failure is a RETURNED result, never a throw :
        // `ok:false` is the machine failure flag every consumer and
        // client branches on.
        s["code"] = json!(code);
        s["errorKind"] = json!(kind);
        s["ok"] = json!(false);
        if s.get("url").is_some() {
            s["content_ok"] = json!(false);
            s["read_status"] = json!(if kind == "walled" {
                "walled"
            } else if code == "content.notfound" {
                "notfound"
            } else if code == "content.incomplete" {
                "incomplete"
            } else {
                "error"
            });
            s["content_complete"] = json!(false);
            if code == "content.notfound"
                && let Some(query) = s["url"].as_str().and_then(moved_page_query)
            {
                s["suggested_query"] = json!(query);
            }
        }
        v["structuredContent"] = s;
    }
    v
}

/// Use readable path words to locate a moved page. Never echo credentials,
/// query parameters, fragments, or opaque percent-encoded path segments.
fn moved_page_query(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    let host = url.host_str()?;
    let words: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.contains('%'))
        .flat_map(|part| part.split(|c: char| !c.is_alphanumeric()))
        .filter(|word| {
            word.chars().any(char::is_alphabetic)
                && word.len() <= 64
                && !matches!(*word, "html" | "htm" | "php" | "aspx" | "index")
        })
        .take(12)
        .collect();
    (!words.is_empty()).then(|| format!("site:{host} {}", words.join(" ")))
}

/// v4.7 uniform envelope: a classified failure is a NORMAL tool
/// result whose envelope carries `ok:false`; nothing raises. Lives on
/// structuredContent so it survives the TextOnly `[meta]` fold. The
/// one predicate every consumer branches on (MCP wire, batch
/// renderers, shot receipts, CLI exit codes).
pub(crate) fn is_failure(result: &Value) -> bool {
    result
        .pointer("/structuredContent/ok")
        .and_then(Value::as_bool)
        == Some(false)
}

/// The tool error for a call whose arguments fall outside the
/// spec, with code `<cli_cmd>.invalid` (`fetch.invalid`, …). `None`
/// when the arguments pass or the spec does not list the tool.
pub(super) fn invalid_args_error(name: &str, args: &Value) -> Option<Value> {
    let tool = crate::spec::TOOLS.iter().find(|t| t.name == name)?;
    let problem = crate::spec::check_args(tool, args).err()?;
    let cmd = tool.cli_cmd;
    Some(tool_error_structured(
        format!("{cmd}: {problem}"),
        "permanent",
        Some(json!({
            "code": format!("{cmd}.invalid"),
            "next_action": "correct the parameter using the current tools/list schema, or omit an optional parameter",
        })),
    ))
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
            "network failure : a retry may work; if repeated, check this host from another network or choose another source".into()
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
    pub(super) browser_document: Option<crate::ghost::document::Document>,
}

tokio::task_local! {
    /// A per-call witness survives cancellation of the fetch future. Ordinary
    /// traces keep their own ordered steps; recursive adapters cannot replace
    /// the evidence already observed by a deadline-bound caller.
    pub(super) static FETCH_TRACE: std::sync::Arc<std::sync::Mutex<Vec<Value>>>;
}

impl Trace {
    pub(super) fn observe_browser(&mut self, document: &crate::ghost::document::Document) {
        self.browser_document = Some(document.clone());
        self.step(
            "2",
            "browser-document",
            &format!(
                "generation={} status={}",
                document.generation,
                document
                    .status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unavailable".into())
            ),
            0,
        );
    }
    pub(super) fn step(&mut self, tier: &str, action: &str, outcome: &str, ms: u128) {
        let step = json!({
            "tier": tier,
            "action": action,
            "outcome": outcome,
            "ms": ms,
        });
        let _ = FETCH_TRACE.try_with(|witness| {
            witness
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(step.clone());
        });
        self.steps.push(step);
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
        // A resolver that did not answer is as retryable as a connect
        // that did not: the same name can resolve a second later. A name
        // that does not exist (Dns) stays permanent, like an Ssrf block.
        FetchError::Timeout | FetchError::Io(_) | FetchError::DnsTimeout(_) => "transient",
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
        FetchError::Tls(msg) if crate::transport::tls::is_cert_verify_failure(msg) => "tls.verify",
        _ => "permanent",
    }
}

/// Wire-level evidence for the per-host repeated-failure advice
/// (`recent_network_failures`). The retryable kinds qualify outright;
/// the `Http` protocol variant is the pipe itself dying mid-exchange
/// (headers never arrived, message truncated), the same story for the
/// caller. Policy (`Ssrf`, `InvalidUrl`), name (`Dns`) and site
/// (`TooManyRedirects`) failures never count: the wire delivered an
/// answer, the answer was "no".
pub(super) fn transport_failure_evidence(e: &FetchError) -> bool {
    fetch_error_kind(e) == "transient" || matches!(e, FetchError::Http(_))
}

/// The stable machine code for a transport failure, taken from the
/// error's own variant instead of its prose. `None` means the variant
/// carries no code of its own and the text classifier decides.
///
/// This exists because prose-matching is a trap (#248): the guard's DNS
/// messages ended in "fail-closed SSRF guard", so a host that does not
/// exist came back as `guard.ssrf`, and an agent branching on that code
/// concluded the target was forbidden by policy.
pub(super) fn fetch_error_code(e: &FetchError) -> Option<&'static str> {
    match e {
        // A name that does not resolve and a resolver that does not
        // answer are both name failures; the KIND carries the retry
        // signal (DnsTimeout is transient).
        FetchError::Dns(_) | FetchError::DnsTimeout(_) => Some("network.dns"),
        FetchError::Ssrf(_) => Some("guard.ssrf"),
        FetchError::ProxyConfig(_) => Some("proxy.config"),
        _ => None,
    }
}

/// Machine class for a transport-level fetch failure, recorded in
/// the result state so callers can distinguish site failures from network
/// failures. Mirrors friendly_fetch_error; the strings are API surface.
pub(super) fn transport_class(e: &FetchError) -> &'static str {
    match e {
        FetchError::Timeout => "timeout",
        FetchError::TooManyRedirects => "too_many_redirects",
        FetchError::Dns(_) => "dns",
        FetchError::DnsTimeout(_) => "dns_timeout",
        FetchError::Ssrf(_) => "ssrf",
        FetchError::InvalidUrl(_) => "invalid_url",
        FetchError::Ghost(_) => "ghost",
        FetchError::Http(_) => "protocol",
        FetchError::ProxyConfig(_) => "configuration",
        FetchError::Tls(msg) => {
            let m = msg.to_lowercase();
            if m.contains("reset") || m.contains("eof") {
                "reset"
            } else {
                "tls"
            }
        }
        FetchError::Io(err) => {
            let m = err.to_string().to_lowercase();
            if m.contains("refused") {
                "refused"
            } else if m.contains("timed out") {
                "timeout"
            } else if m.contains("not found") || m.contains("no address") || m.contains("not known")
            {
                // getaddrinfo: "Name or service not known" (Linux),
                // "nodename nor servname provided, or not known" (macOS).
                "dns"
            } else if m.contains("reset") {
                "reset"
            } else {
                "network"
            }
        }
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

    // The repeated-failure advice counts wire evidence, not only the
    // retryable kinds: a connection that dies mid-exchange is `Http`
    // (the live broken-wire fixture), while policy and name failures
    // are answers from the wire, not evidence about it (#248 family).
    #[test]
    pub(super) fn wire_failures_feed_the_repeated_failure_advice() {
        assert!(transport_failure_evidence(&FetchError::Http(
            "eof before headers".into()
        )));
        assert!(transport_failure_evidence(&FetchError::Timeout));
        assert!(transport_failure_evidence(&FetchError::DnsTimeout(
            "the resolver did not answer".into()
        )));
        assert!(transport_failure_evidence(&FetchError::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer")
        )));
        assert!(!transport_failure_evidence(&FetchError::Ssrf(
            "10.0.0.1 is a private/loopback address".into()
        )));
        assert!(!transport_failure_evidence(&FetchError::Dns(
            "no such host".into()
        )));
        assert!(!transport_failure_evidence(&FetchError::InvalidUrl(
            "not a url".into()
        )));
        assert!(!transport_failure_evidence(&FetchError::TooManyRedirects));
    }

    #[test]
    fn stealth_v3_invalid_proxy_is_configuration_not_network_evidence() {
        // Nextest owns the process-global configuration for this test.
        let directory =
            std::env::temp_dir().join(format!("donsetch-proxy-errors-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let config = directory.join("config.toml");
        std::fs::write(&config, "[proxy]\nfrom_environment=true\n").unwrap();
        unsafe {
            std::env::set_var("DONSETCH_CONFIG", config);
            std::env::remove_var("DONSETCH_NO_ENV_PROXY");
            std::env::set_var(
                "HTTP_PROXY",
                "socks4://alice:private-token@broken.invalid:1080",
            );
            std::env::set_var("NO_PROXY", "");
            std::env::remove_var("no_proxy");
        }
        let error = crate::transport::proxy::from_env_for("http://example.com/").unwrap_err();
        assert!(
            !transport_failure_evidence(&error),
            "a configuration error must not teach a network failure"
        );
        assert_eq!(fetch_error_code(&error), Some("proxy.config"));
        let response = tool_error_structured(
            friendly_fetch_error(&error),
            fetch_error_kind(&error),
            Some(json!({"url":"http://example.com/", "code":fetch_error_code(&error)})),
        );
        assert_eq!(response["errorKind"], "permanent");
        assert_eq!(response["structuredContent"]["code"], "proxy.config");
        assert_eq!(response["structuredContent"]["retry_safe"], false);
        let action = response["structuredContent"]["next_action"]
            .as_str()
            .unwrap();
        assert!(
            action.contains("proxy") && action.contains("config"),
            "{action}"
        );
        assert!(
            !response.to_string().contains("private-token")
                && !response.to_string().contains("alice")
        );
        std::fs::remove_dir_all(directory).unwrap();
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
    #[test]
    fn report_audit_all_errors_supply_machine_state_and_next_action() {
        let raw = tool_error("invalid URL: not-a-url");
        assert_eq!(raw["isError"], false);
        assert_eq!(raw["structuredContent"]["ok"], false);
        assert_eq!(raw["structuredContent"]["code"], "fetch.invalid");
        assert!(
            !raw["structuredContent"]["next_action"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        let auth = tool_error("authentication required at https://example.com (HTTP 200)");
        assert_eq!(auth["structuredContent"]["code"], "wall.auth");
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn stealth_v3_incomplete_document_is_retryable_without_invented_notfound() {
        let result = tool_error_structured(
            "browser document incomplete: content did not settle",
            "transient",
            Some(json!({"url":"https://owned.test/empty", "status":200,
                "verdict":"Incomplete"})),
        );
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["ok"], false);
        assert_eq!(result["errorKind"], "transient");
        assert_eq!(result["code"], "content.incomplete");
        let state = &result["structuredContent"];
        assert_eq!(state["read_status"], "incomplete");
        assert_eq!(state["status"], 200);
        assert_eq!(state["content_ok"], false);
        assert_eq!(state["content_complete"], false);
        assert!(state.get("suggested_query").is_none());
        assert!(!state["next_action"].as_str().unwrap().is_empty());
        assert_eq!(error_code("not found: /missing", None), "content.notfound");
        assert_eq!(
            error_code("walled", Some(&json!({"verdict":"Challenge"}))),
            "wall.challenge"
        );
    }

    #[test]
    fn stealth_v3_browser_transport_failure_is_not_content_or_previous_http_wall() {
        for (message, expected) in [
            (
                "browser navigation error: ghost: cdp link closed while reading browser DOM: transport=websocket protocol error, browser=signal: 5 (SIGTRAP), generation=4",
                "browser.transport",
            ),
            (
                "browser navigation error: ghost: cdp dropped: Page.navigate (websocket eof)",
                "browser.transport",
            ),
            (
                "browser automation error: ghost: cdp send: Connection reset",
                "browser.transport",
            ),
            (
                "browser navigation error: ghost: cdp timeout: Page.navigate",
                "browser.timeout",
            ),
            (
                "browser launch failed: ghost: cdp connect: IO error: Connection refused",
                "browser.transport",
            ),
            (
                "browser launch failed: ghost: cdp connect: ws handshake timeout",
                "browser.timeout",
            ),
        ] {
            let result = tool_error_structured(
                message,
                "permanent",
                Some(
                    json!({"url":"https://owned.test/", "status":null, "verdict":"Challenge(Cloudflare)", "next_action":"check the URL; the site may be blocking"}),
                ),
            );
            assert_eq!(result["code"], expected, "{message}");
            assert_eq!(result["structuredContent"]["code"], expected);
            assert_eq!(result["structuredContent"]["read_status"], "error");
            assert_eq!(result["structuredContent"]["content_ok"], false);
            assert_eq!(result["errorKind"], "transient");
            let action = result["structuredContent"]["next_action"].as_str().unwrap();
            assert!(action.contains("browser"), "{action}");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(action));
            assert!(!text.contains("site may be blocking"));
        }
        assert_eq!(
            error_code("interactive captcha requires a human", None),
            "wall.captcha"
        );
        assert_eq!(
            error_code("SSRF guard: private/loopback", None),
            "guard.ssrf"
        );
        let action_failure = tool_error_structured(
            "actions[0] failed: ghost: cdp link closed",
            "transient",
            Some(json!({"url":"https://owned.test/", "retry_safe":false})),
        );
        assert_eq!(action_failure["errorKind"], "permanent");
        assert_eq!(action_failure["structuredContent"]["retry_safe"], false);
        assert!(
            action_failure["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("do not replay")
        );
    }

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

    // V24: sniffing reads symptoms, never data : a wall error quoting
    // its URL made the classifier read a rate limit out of the wall's
    // own PORT (live case: the V03 fixture server answered on 42945).
    #[test]
    pub(super) fn a_url_port_is_not_a_rate_limit() {
        let wall = json!({"verdict": "Challenge(Cloudflare)", "status": 403});
        assert_eq!(
            error_code(
                "bot wall: http://127.0.0.1:42945/wall is protected by Cloudflare (try fetch with tier=2 for headless browser)",
                Some(&wall)
            ),
            "wall.challenge"
        );
        // A status that is not glued to other digits still classifies.
        assert_eq!(
            error_code("server answered HTTP 429", None),
            "network.ratelimit"
        );
        assert_eq!(
            error_code("rate limited: too many requests", None),
            "network.ratelimit"
        );
    }

    #[test]
    pub(super) fn url_words_are_not_failure_signal() {
        let wall = json!({"verdict": "Challenge(Cloudflare)", "status": 403});
        assert_eq!(
            error_code(
                "bot wall: https://dns.example.com/x is protected by Cloudflare",
                Some(&wall)
            ),
            "wall.challenge"
        );
        assert_eq!(
            error_code(
                "blocked: https://x.example/retry?timeout=5 returned a wall",
                Some(&wall)
            ),
            "wall.challenge"
        );
        // Genuine transport prose still classifies.
        assert_eq!(
            error_code("dns error: failed to lookup address information", None),
            "network.dns"
        );
        assert_eq!(
            error_code("request timed out after 30s", None),
            "network.timeout"
        );
    }

    #[test]
    pub(super) fn off_list_enum_args_are_a_tool_error() {
        let args = json!({ "url": "https://example.com", "tier": "3" });
        let v = invalid_args_error("web_fetch", &args).expect("tier=3 must be refused");
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["ok"], false);
        assert_eq!(v["errorKind"], "permanent");
        assert_eq!(v["code"], "fetch.invalid");
        assert_eq!(v["structuredContent"]["code"], "fetch.invalid");
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with(r#"fetch: tier must be one of "auto", "1", "2"; got "3""#),
            "{text}"
        );
        assert!(text.contains("Next action:"), "{text}");

        let args = json!({ "url": "https://example.com", "mode": "asdf" });
        let v = invalid_args_error("web_fetch", &args).expect("mode=asdf must be refused");
        assert_eq!(v["code"], "fetch.invalid");
        let args = json!({ "query": "q", "intent": "asdf" });
        let v = invalid_args_error("web_search", &args).expect("intent=asdf must be refused");
        assert_eq!(v["code"], "search.invalid");
        let args = json!({ "url": "https://example.com", "mode": "asdf" });
        let v = invalid_args_error("web_crawl", &args).expect("mode=asdf must be refused");
        assert_eq!(v["code"], "crawl.invalid");
    }

    #[test]
    fn v47_failures_are_returned_envelopes_not_thrown() {
        let v = tool_error_structured(
            "fetch: deadline_ms exceeded at https://owned.test/",
            "transient",
            Some(json!({"url": "https://owned.test/", "code": "deadline.hit"})),
        );
        // Returned, never thrown: the wire shape stays a normal
        // result; the envelope is the failure channel.
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["ok"], false);
        assert_eq!(v["structuredContent"]["code"], "deadline.hit");
        assert_eq!(v["structuredContent"]["errorKind"], "transient");
        assert!(is_failure(&v));
        let ok = json!({"content": [{"type": "text", "text": "page"}], "structuredContent": {"ok": true}});
        assert!(!is_failure(&ok));
        // A success envelope without the field is not a failure either.
        let bare = json!({"content": [{"type": "text", "text": "x"}]});
        assert!(!is_failure(&bare));
    }

    #[test]
    pub(super) fn listed_enum_args_and_unknown_tools_pass_through() {
        let args = json!({ "url": "https://example.com", "tier": "2" });
        assert!(invalid_args_error("web_fetch", &args).is_none());
        assert!(
            invalid_args_error("web_fetch", &json!({ "url": "https://example.com" })).is_none()
        );
        // An unknown tool is the dispatcher's protocol error, not this one.
        assert!(invalid_args_error("web_nope", &json!({ "tier": "3" })).is_none());
    }

    // #248: a host that does not resolve is a NAME failure, and a
    // resolver that does not answer is retryable. Both used to reach the
    // agent as `guard.ssrf` (which reads as "forbidden by policy") and
    // `permanent`, so a name that would resolve on the next try was not
    // retried. The guard's own error variant now sets the code, and the
    // classifier reads that code instead of matching prose.
    #[tokio::test]
    pub(super) async fn a_host_that_does_not_resolve_is_not_a_policy_block() {
        let err =
            crate::fetch::guards::ensure_url_safe("https://no-such-host-for-donsetch.invalid/")
                .await
                .unwrap_err();
        let v = tool_error_structured(
            friendly_fetch_error(&err),
            fetch_error_kind(&err),
            Some(json!({
                "code": fetch_error_code(&err),
                "fetch_error": transport_class(&err),
            })),
        );
        assert_eq!(v["code"], "network.dns", "got {v}");
        assert_eq!(v["errorKind"], "permanent");
        assert_eq!(v["structuredContent"]["fetch_error"], json!("dns"));
        assert!(
            !v["content"][0]["text"].as_str().unwrap().contains("SSRF"),
            "the message must not read as a policy block: {v}"
        );
    }

    #[test]
    pub(super) fn a_resolver_timeout_is_transient() {
        let err = FetchError::DnsTimeout("the resolver did not answer within 5s".into());
        let v = tool_error_structured(
            friendly_fetch_error(&err),
            fetch_error_kind(&err),
            Some(json!({ "code": fetch_error_code(&err) })),
        );
        assert_eq!(v["code"], "network.dns");
        assert_eq!(v["errorKind"], "transient", "a retry can work: {v}");
        assert_eq!(transport_class(&err), "dns_timeout");
    }

    // The SSRF block keeps its code: this is the case `guard.ssrf` is for.
    #[test]
    pub(super) fn a_private_address_is_still_a_policy_block() {
        let err = FetchError::Ssrf("10.0.0.1 is a private/loopback address : SSRF guard".into());
        let v = tool_error_structured(
            friendly_fetch_error(&err),
            fetch_error_kind(&err),
            Some(json!({ "code": fetch_error_code(&err) })),
        );
        assert_eq!(v["code"], "guard.ssrf");
        assert_eq!(v["errorKind"], "permanent");
        assert_eq!(transport_class(&err), "ssrf");
    }

    // The text classifier is the fallback for every error that carries no
    // code of its own, so both paths must read the same failure the same
    // way: a DNS failure is network.dns, a policy block is guard.ssrf.
    #[test]
    pub(super) fn the_text_fallback_agrees_with_the_typed_code() {
        let dns = FetchError::Dns("could not resolve x.invalid: no such host".into());
        assert_eq!(
            error_code(&friendly_fetch_error(&dns), None).as_ref(),
            "network.dns"
        );
        let ssrf = FetchError::Ssrf("10.0.0.1 is a private/loopback address : SSRF guard".into());
        assert_eq!(
            error_code(&friendly_fetch_error(&ssrf), None).as_ref(),
            "guard.ssrf"
        );
    }

    // #282: the two content-quality failures keep their own codes so
    // an agent can tell "the page is a shell/login wall" from "the
    // challenge never cleared".
    #[test]
    pub(super) fn content_quality_walls_get_distinct_codes() {
        assert_eq!(
            error_code(
                "blocked at https://x : the page is an anti-bot challenge that did not clear (the extracted text is the interstitial, not content)",
                None
            )
            .as_ref(),
            "wall.challenge_unsolved"
        );
        assert_eq!(
            error_code(
                "blocked at https://x : the page rendered only navigation and login chrome (an empty shell or a login wall), no content",
                None
            )
            .as_ref(),
            "wall.empty_shell"
        );
    }
}

#[cfg(test)]
mod v471_tests {
    use super::*;
    #[test]
    fn v471_dead_page_suggests_topic_without_url_secrets() {
        let url = "https://docs.example.com/guides/rust-async.html?token=secret#private";
        let result = tool_error_structured(
            format!("not found: {url} returned HTTP 404"),
            "permanent",
            Some(json!({"url":url})),
        );
        let query = result["structuredContent"]["suggested_query"]
            .as_str()
            .expect("useful topic query");
        assert_eq!(query, "site:docs.example.com guides rust async");
        assert!(!query.contains("secret") && !query.contains("private"));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn v471_argument_types_and_unknown_keys_are_rejected() {
        for args in [
            json!([]),
            json!({"deadline_ms":"500"}),
            json!({"deadline_ms":-1}),
            json!({"focus":true}),
            json!({"toc":"false"}),
            json!({"url":[1]}),
            json!({"actions":{}}),
            json!({"archive":"only"}),
            json!({"url":vec!["https://example.com";13]}),
        ] {
            let result = invalid_args_error("web_fetch", &args)
                .expect("invalid arguments must not silently use defaults");
            assert_eq!(result["structuredContent"]["code"], "fetch.invalid");
            assert_eq!(result["structuredContent"]["ok"], false);
        }
        assert!(
            invalid_args_error(
                "web_fetch",
                &json!({"url":"https://example.com","deadline_ms":500,"toc":false,"focus":null})
            )
            .is_none()
        );
    }
    #[test]
    fn v471_topic_hint_is_absent_for_roots_and_opaque_paths() {
        for url in [
            "https://example.com/?token=secret",
            "https://example.com/index.html",
            "https://example.com/%E6%97%A5?token=secret",
        ] {
            assert_eq!(moved_page_query(url), None);
        }
    }
}

#[cfg(test)]
mod transport_class_tests {
    #[test]
    fn transport_classes_separate_death_from_ambiguity() {
        use super::transport_class;
        use crate::error::FetchError;
        // Certificate, DNS and connection failures retain distinct classes.
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
        // Timeouts and protocol failures retain their own retry signals.
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
}
