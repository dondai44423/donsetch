//! The product's fetches over the v5 core (`v5core::Session`).
//!
//! Two entry points, the same two the product has always had:
//! - a document (`Fetcher::fetch*`) is an encounter: the identity this origin
//!   accepts, the state it earned, the page cache, the site's own interface and
//!   the in-process challenge engine;
//! - a one-shot read (`Fetcher::fetch_once*`) is one request exactly as the
//!   caller describes it: an API, a search page, a file.
//!
//! The product keeps what is the product's: the URL/SSRF gate on the first URL
//! and on every redirect hop (asked before the hop is dialed, crawl scope and
//! robots included), egress lanes, wall scoring and the outcome surface. v5 is
//! blocking, so every call runs under `spawn_blocking`; one `Session` is shared
//! by all of them and folds what each call learned into one state, kept under
//! the product's cache root (`<cache_dir>/v5/`).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::detect::walls::{self, Vendor, Verdict};
use crate::egress::proxy::Proxy;
use crate::egress::request_route::RequestRoute;
use crate::error::FetchError;
use crate::fetch::client::{CacheState, FetchOutcome, RedirectGate};
use crate::profile::RequestClass;

/// A document's whole budget: identity search, challenge engine and the
/// site's own interface included. Callers wrap their own deadlines around it.
const DOCUMENT_DEADLINE: Duration = Duration::from_secs(20);

/// A one-shot read's ceiling; callers wrap tighter deadlines around it. A file
/// (the update tarball) is the longest thing read this way.
const ONCE_TIMEOUT: Duration = Duration::from_secs(300);

/// The identity a one-shot read presents: a real client profile from the
/// platform this binary runs on. A Linux user agent leaving a Windows
/// machine (or a Mac) is exactly the incoherence the identity rule forbids,
/// and the client hints would contradict it too.
#[cfg(target_os = "windows")]
const ONCE_IDENTITY: &str = "chrome-win";
#[cfg(target_os = "macos")]
const ONCE_IDENTITY: &str = "safari";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const ONCE_IDENTITY: &str = "chrome-local";

/// A request-local legacy client (a User-Agent the caller names) rides the
/// identity that claims no browser: it carries no client hints and no fetch
/// metadata to contradict the agent string.
const LEGACY_IDENTITY: &str = "plain";

pub struct V5Fetcher {
    session: Arc<v5core::Session>,
}

/// A document request as the product describes it.
pub struct Document<'a> {
    pub url: &'a str,
    pub proxy: Option<Proxy>,
    pub route: RequestRoute,
    /// Spend and earn learned state (cookies, scores, the page cache).
    pub learn: bool,
    /// Neither read nor write the page cache (a probe: a cached page is not
    /// evidence about the wall right now).
    pub skip_cache: bool,
    /// The page this navigation was reached from (a crawl's link).
    pub referer: Option<&'a str>,
    /// Asked before every redirect hop is dialed (crawl scope and robots).
    pub gate: Option<RedirectGate>,
}

/// A one-shot request as the product describes it.
pub struct OneShot<'a> {
    pub url: &'a str,
    pub conditional: &'a [(String, String)],
    pub proxy: Option<&'a Proxy>,
    pub use_jar: bool,
    pub referer: Option<&'a str>,
    pub class: RequestClass,
    /// A request-local legacy client, by its User-Agent.
    pub user_agent: Option<&'a str>,
}

impl V5Fetcher {
    /// `persist` keeps v5's learned state (scores, cookies, credentials) and its
    /// page cache on disk under the product's cache root (`v5/`), so
    /// `DONSETCH_CACHE_DIR` and `[paths] cache_dir` move them with everything else.
    pub fn new(persist: bool) -> Self {
        let session = if persist {
            v5core::Session::at(state_path())
        } else {
            v5core::Session::new(false)
        };
        Self {
            session: Arc::new(session),
        }
    }

    /// One document: the encounter.
    pub async fn document(&self, document: Document<'_>) -> Result<FetchOutcome, FetchError> {
        crate::fetch::guards::ensure_url_safe(document.url).await?;
        let refusal: Arc<Mutex<Option<FetchError>>> = Arc::default();
        let mut request = v5core::Request::new(document.url);
        request.proxy = document.proxy.as_ref().map(Proxy::wire_url);
        request.learn = document.learn;
        request.cache = document.learn && !document.skip_cache;
        request.deadline = DOCUMENT_DEADLINE;
        let cancel = v5core::Cancel::new();
        request.policy = v5core::Policy {
            hop: Some(hop_guard(Arc::clone(&refusal), document.gate)),
            navigation: navigation_headers(document.referer, document.url),
            cancel: Some(cancel.clone()),
            ..v5core::Policy::default()
        };
        let started = Instant::now();
        let session = Arc::clone(&self.session);
        let outcome = blocking(cancel, move || session.fetch(&request)).await;
        let mut out = into_outcome(outcome, document.route, &refusal)?;
        out.elapsed = started.elapsed();
        Ok(out)
    }

    /// One request exactly as described: no identity search, no page cache, no
    /// engine. The answer is the reply, whatever its status.
    pub async fn once(&self, shot: OneShot<'_>) -> Result<FetchOutcome, FetchError> {
        if let Some(agent) = shot.user_agent
            && (agent.is_empty()
                || !agent.is_ascii()
                || agent.bytes().any(|b| b.is_ascii_control()))
        {
            return Err(FetchError::Http("invalid User-Agent".into()));
        }
        crate::fetch::guards::ensure_url_safe(shot.url).await?;
        let mut headers: Vec<(String, String)> = Vec::new();
        let kind = if shot.class == RequestClass::Navigation {
            v5core::Kind::Navigate
        } else {
            let (accept, dest) = shot.class.accept_and_dest();
            headers.push(("accept".into(), accept));
            headers.push((
                "sec-fetch-mode".into(),
                if dest == "font" { "cors" } else { "no-cors" }.into(),
            ));
            headers.push(("sec-fetch-dest".into(), dest));
            v5core::Kind::Subresource
        };
        if let Some(agent) = shot.user_agent {
            headers.push(("user-agent".into(), agent.to_string()));
        }
        match shot.referer {
            Some(referer) => headers.extend(navigation_headers(Some(referer), shot.url)),
            // A subresource always has an initiator; with none named it is the
            // page's own origin.
            None if kind == v5core::Kind::Subresource => {
                headers.push(("sec-fetch-site".into(), "same-origin".into()))
            }
            None => {}
        }
        headers.extend(shot.conditional.iter().cloned());
        if headers.iter().any(|(name, value)| {
            !crate::fetch::guards::valid_header_value(name)
                || !crate::fetch::guards::valid_header_value(value)
        }) {
            return Err(FetchError::Http(
                "invalid header value (CR/LF/NUL) : refused to send".into(),
            ));
        }
        let refusal: Arc<Mutex<Option<FetchError>>> = Arc::default();
        let cancel = v5core::Cancel::new();
        let once = v5core::Once {
            identity: Some(
                if shot.user_agent.is_some() {
                    LEGACY_IDENTITY
                } else {
                    ONCE_IDENTITY
                }
                .to_string(),
            ),
            proxy: shot.proxy.map(Proxy::wire_url),
            kind,
            headers,
            cookies: shot.use_jar,
            timeout: ONCE_TIMEOUT,
            cap: None,
            policy: v5core::Policy {
                hop: Some(hop_guard(Arc::clone(&refusal), None)),
                request_max: Duration::MAX,
                cancel: Some(cancel.clone()),
                ..v5core::Policy::default()
            },
            ..v5core::Once::new(shot.url)
        };
        let route = shot
            .proxy
            .cloned()
            .map(RequestRoute::pinned)
            .unwrap_or_else(RequestRoute::direct);
        let started = Instant::now();
        let session = Arc::clone(&self.session);
        let reply = blocking(cancel, move || session.once(&once))
            .await
            .map_err(|raw| refused_or(raw, &refusal))?;
        let verdict = walls::detect(reply.status, &reply.headers, &reply.bytes);
        Ok(FetchOutcome {
            route,
            url: reply.url,
            status: reply.status,
            alpn: format!("v5:once:{}", reply.identity),
            headers: reply.headers,
            body: reply.bytes,
            redirects: 0,
            cache: CacheState::None,
            used_pool: false,
            verdict,
            elapsed: started.elapsed(),
            verified: false,
        })
    }

    /// The names of the cookies any identity holds for `host`.
    pub fn cookie_names(&self, host: &str) -> Vec<String> {
        let mut names: Vec<String> = v5core::identity::names()
            .into_iter()
            .flat_map(|identity| self.session.cookie_names(host, identity))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Where v5 keeps its state file (its page cache is `pages/` beside it).
pub fn state_path() -> std::path::PathBuf {
    crate::paths::cache_dir().join("v5").join("state.json")
}

/// The blocking session call on a blocking thread, never on a runtime worker,
/// so peers on the same runtime (in-process servers, the robots gate) keep
/// being polled while it runs.
/// Run a v5 call off the async runtime. Dropping the returned future (a
/// caller's timeout, an aborted task, a retiring daemon) flips `cancel`, so the
/// request in flight is dropped with its connection instead of running on.
async fn blocking<F, R>(cancel: v5core::Cancel, job: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    struct OnDrop(v5core::Cancel);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel_on_drop = OnDrop(cancel);
    match tokio::task::spawn_blocking(job).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => panic!("v5 worker task failed: {error}"),
    }
}

/// The product's redirect rule for every hop, asked before it is dialed: the
/// same scheme / credentials / rules / private-address gate as the first URL,
/// then the caller's own gate (crawl scope and robots). A refused hop records
/// its typed error in `refusal`; a hop the gate declines ends the chain on the
/// 3xx, which the caller reads as the honest skip.
fn hop_guard(
    refusal: Arc<Mutex<Option<FetchError>>>,
    gate: Option<RedirectGate>,
) -> v5core::HopGuard {
    let handle = tokio::runtime::Handle::try_current().ok();
    Arc::new(move |previous: &str, next: &str| {
        let Ok(base) = url::Url::parse(previous) else {
            return v5core::Hop::Deny(next.to_string());
        };
        match crate::fetch::guards::validate_redirect_url(&base, next) {
            Ok(url) => match (&gate, &handle) {
                // The session runs on a blocking thread, outside the runtime:
                // waiting on the async gate here is allowed and parks only it.
                (Some(gate), Some(handle)) if !handle.block_on(gate(url)) => v5core::Hop::Stop,
                _ => v5core::Hop::Follow,
            },
            Err(FetchError::NonHttpRedirect(_)) => v5core::Hop::Stop,
            Err(error) => {
                *refusal.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
                v5core::Hop::Deny(next.to_string())
            }
        }
    })
}

/// A link-initiated navigation's `referer` (Chrome's default
/// `strict-origin-when-cross-origin`) and its `sec-fetch-site`.
fn navigation_headers(referer: Option<&str>, target: &str) -> Vec<(String, String)> {
    let Some(referer) = referer else {
        return Vec::new();
    };
    vec![
        (
            "referer".into(),
            crate::fetch::client::referer_value(referer, target),
        ),
        (
            "sec-fetch-site".into(),
            crate::fetch::client::fetch_site(referer, target).into(),
        ),
    ]
}

/// A failure's typed error: the refusal the hop guard recorded, or the class
/// v5 named in the message's prefix.
fn refused_or(raw: String, refusal: &Mutex<Option<FetchError>>) -> FetchError {
    if raw.starts_with("denied:")
        && let Some(error) = refusal.lock().unwrap_or_else(|p| p.into_inner()).take()
    {
        return error;
    }
    map_error(raw)
}

/// v5's error classes (`timeout:`, `dns:`, `tls:`, `proxy:`, `connect:`,
/// `body:`, `request:`, `redirect:`, `denied:`) onto the product's taxonomy.
/// An unrecognised shape stays an honest `Http` message.
fn map_error(raw: String) -> FetchError {
    let (class, detail) = raw
        .split_once(':')
        .map_or(("", raw.as_str()), |(class, detail)| (class, detail.trim()));
    match class {
        "dns" => FetchError::Dns(detail.to_owned()),
        "timeout" => FetchError::Timeout,
        "tls" => FetchError::Tls(detail.to_owned()),
        "connect" => FetchError::Io(std::io::Error::other(detail.to_owned())),
        "proxy" => {
            // A proxy demanding credentials keeps the auth-fail shape the
            // lane health reads; anything else is a dead lane.
            let lower = detail.to_ascii_lowercase();
            if lower.contains("407") || lower.contains("authentication required") {
                FetchError::Http(format!("CONNECT -> 407 ({detail})"))
            } else {
                FetchError::Io(std::io::Error::other(format!("proxy: {detail}")))
            }
        }
        "denied" => match crate::fetch::guards::validate_url_basic(detail) {
            Err(error) => error,
            Ok(_) => FetchError::Http(format!("denied: {detail}")),
        },
        "redirect" => FetchError::TooManyRedirects,
        "body" | "request" | "identity" => FetchError::Http(format!("{class}: {detail}")),
        _ => FetchError::Http(raw),
    }
}

/// v5's outcome as the product's. The product's own wall detector reads the
/// body, as on every path; v5's wall class wins over a body-only `ContentOk`.
fn into_outcome(
    outcome: v5core::Outcome,
    route: RequestRoute,
    refusal: &Mutex<Option<FetchError>>,
) -> Result<FetchOutcome, FetchError> {
    if outcome.class == v5core::Class::Network && outcome.status == 0 {
        // No answer at all: the last attempt's failure names the class. An
        // encounter that ran out of budget before any request is a timeout.
        let reason = outcome
            .failures
            .last()
            .cloned()
            .unwrap_or_else(|| "timeout: no attempt fit the deadline".into());
        return Err(refused_or(reason, refusal));
    }
    if outcome.status == 304 {
        // v5 consumes the revalidations it asked for; a 304 reaching here is
        // unsolicited and has no body to stand for.
        return Err(FetchError::Http(
            "304 response without a matching request validator snapshot".into(),
        ));
    }
    // A data-path or embedded answer is the records, not the document they came
    // from (an API body, or a shell): hand over the rendered records as text.
    let records = outcome.transport != "document" && !outcome.body_text.is_empty();
    let (headers, body) = if records {
        (
            vec![(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            outcome.body_text.into_bytes(),
        )
    } else {
        let body = match outcome.bytes {
            Some(bytes) => bytes,
            None => outcome.body.unwrap_or_default().into_bytes(),
        };
        (outcome.headers, body)
    };
    let verdict = verdict_for(
        outcome.class,
        outcome.wall.as_ref().map(|wall| wall.vendor.as_str()),
        walls::detect(outcome.status, &headers, &body),
    );
    Ok(FetchOutcome {
        route,
        url: outcome.final_url,
        status: outcome.status,
        alpn: format!(
            "v5:{}:{}",
            outcome.transport,
            outcome.identity.as_deref().unwrap_or("-")
        ),
        headers,
        body,
        redirects: 0,
        cache: match outcome.cache.as_deref() {
            Some("fresh") => CacheState::Fresh,
            Some("revalidated") => CacheState::Revalidated,
            _ => CacheState::None,
        },
        used_pool: false,
        verdict,
        elapsed: Duration::from_secs_f64(outcome.ms_total.max(0.0) / 1000.0),
        verified: outcome.class == v5core::Class::Content && verdict == Verdict::ContentOk,
    })
}

/// v5's class decides bot walls: it read the answer against a contract and
/// knows which identity got through, while the product's markers also fire on
/// the passive vendor scripts real pages carry (Medium's content page ships
/// Cloudflare's). Login walls, paywalls and soft 404s stay the product's call.
fn verdict_for(class: v5core::Class, wall_vendor: Option<&str>, detected: Verdict) -> Verdict {
    match (class, detected) {
        (v5core::Class::Content, Verdict::Challenge(_) | Verdict::Blocked) => Verdict::ContentOk,
        (class, Verdict::ContentOk) if is_wall(class) => Verdict::Challenge(vendor(wall_vendor)),
        (_, detected) => detected,
    }
}

fn is_wall(class: v5core::Class) -> bool {
    use v5core::Class::*;
    matches!(
        class,
        WallState | WallProgram | WallContract | WallInteraction | WallIdentity
    )
}

fn vendor(name: Option<&str>) -> Vendor {
    match name.unwrap_or_default() {
        "cloudflare" | "turnstile" => Vendor::Cloudflare,
        "datadome" => Vendor::DataDome,
        "akamai" => Vendor::Akamai,
        "kasada" => Vendor::Kasada,
        "perimeterx" => Vendor::PerimeterX,
        "imperva" => Vendor::Imperva,
        _ => Vendor::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(class: v5core::Class, status: u16) -> v5core::Outcome {
        v5core::Outcome {
            url: "https://a.test/".into(),
            host: "a.test".into(),
            final_url: "https://a.test/".into(),
            class,
            identity: Some("chrome".into()),
            status,
            ms_total: 1.0,
            ms_first: 1.0,
            attempts: Vec::new(),
            title: String::new(),
            text_chars: 0,
            wall: None,
            evidence: Vec::new(),
            failures: Vec::new(),
            spent_state: false,
            transport: "document".into(),
            items: 0,
            sample: Vec::new(),
            sample_urls: Vec::new(),
            body_text: String::new(),
            body: Some(article()),
            headers: Vec::new(),
            bytes: None,
            cache: None,
        }
    }

    fn article() -> String {
        format!(
            "<html><head><title>Fixture</title></head><body><article>{}</article></body></html>",
            "Words a reader came for. ".repeat(30)
        )
    }

    fn map(outcome: v5core::Outcome) -> Result<FetchOutcome, FetchError> {
        into_outcome(outcome, RequestRoute::direct(), &Mutex::new(None))
    }

    #[test]
    fn a_v5_wall_overrides_a_body_only_content_verdict() {
        let mut walled = outcome(v5core::Class::WallProgram, 200);
        walled.wall = Some(v5core::Wall {
            vendor: "cloudflare".into(),
            marker: "cf_chl_opt".into(),
            asks: "managed challenge (page program)",
        });
        assert_eq!(
            map(walled).expect("outcome").verdict,
            Verdict::Challenge(Vendor::Cloudflare)
        );
        let content = map(outcome(v5core::Class::Content, 200)).expect("outcome");
        assert_eq!(content.verdict, Verdict::ContentOk);
        assert_eq!(content.body, article().into_bytes());
    }

    #[test]
    fn verified_content_is_not_a_challenge_for_a_passive_vendor_script() {
        // A content page that ships Cloudflare's passive bot-management script.
        let page = format!(
            "<html><head><title>Medium</title><script src=\"/cdn-cgi/challenge-platform/scripts/jsd/main.js\"></script></head><body><article>{}</article></body></html>",
            "Stories a reader came for. ".repeat(30)
        );
        let content = v5core::Class::Content;
        let detected = walls::detect(200, &[], page.as_bytes());
        assert_eq!(
            verdict_for(content, None, Verdict::Challenge(Vendor::Cloudflare)),
            Verdict::ContentOk,
            "product markers alone said {detected:?}"
        );
        // The product's own judgments that are not bot walls stand.
        for kept in [Verdict::AuthWall, Verdict::Paywall, Verdict::SoftNotFound] {
            assert_eq!(verdict_for(content, None, kept), kept);
        }
        // An answer v5 did not verify keeps the product's wall verdict.
        assert_eq!(
            verdict_for(
                v5core::Class::Incomplete,
                None,
                Verdict::Challenge(Vendor::Cloudflare)
            ),
            Verdict::Challenge(Vendor::Cloudflare)
        );
    }

    #[test]
    fn the_cache_state_reaches_the_product() {
        // A page served from v5's cache without a request must reach the
        // crawler as `Fresh`: it skips pacing on exactly that.
        for (state, expected) in [
            (Some("fresh"), CacheState::Fresh),
            (Some("revalidated"), CacheState::Revalidated),
            (Some("changed"), CacheState::None),
            (None, CacheState::None),
        ] {
            let mut served = outcome(v5core::Class::Content, 200);
            served.cache = state.map(str::to_string);
            assert_eq!(map(served).expect("outcome").cache, expected, "{state:?}");
        }
    }

    #[test]
    fn records_reach_the_product_as_text_not_the_shell_they_came_from() {
        let mut data = outcome(v5core::Class::Content, 200);
        data.transport = "data".into();
        data.body = Some("<html><div id=app></div></html>".into());
        data.headers = vec![("content-type".into(), "text/html".into())];
        data.body_text = format!(
            "# Feed (6 records)\n\n{}",
            "- A caption a reader came for, about the river\n".repeat(6)
        );
        let mapped = map(data).expect("outcome");
        assert!(String::from_utf8_lossy(&mapped.body).starts_with("# Feed"));
        assert!(
            mapped
                .headers
                .iter()
                .any(|(name, value)| name == "content-type" && value.starts_with("text/plain"))
        );
        assert_eq!(mapped.alpn, "v5:data:chrome");
    }

    #[test]
    fn no_answer_is_an_error_named_by_its_class() {
        for (failure, expected) in [
            ("timeout: operation timed out", "Timeout"),
            ("dns: failed to lookup address", "Dns"),
            ("tls: certificate verify failed", "Tls"),
            ("connect: connection refused", "Io"),
            ("redirect: more than 10 redirects", "TooManyRedirects"),
            ("proxy: 407 Proxy Authentication Required", "Http"),
        ] {
            let mut failed = outcome(v5core::Class::Network, 0);
            failed.failures = vec![failure.into()];
            let error = map(failed).err().expect("an error");
            assert!(
                format!("{error:?}").starts_with(expected),
                "{failure}: {error:?}"
            );
        }
        let mut proxy = outcome(v5core::Class::Network, 0);
        proxy.failures = vec!["proxy: 407 Proxy Authentication Required".into()];
        assert!(
            map(proxy)
                .err()
                .unwrap()
                .to_string()
                .contains("CONNECT -> 407")
        );
        // Nothing was even attempted inside the budget.
        assert!(matches!(
            map(outcome(v5core::Class::Network, 0)),
            Err(FetchError::Timeout)
        ));
    }

    #[test]
    fn a_refused_hop_comes_back_as_the_typed_refusal() {
        let refusal = Mutex::new(Some(FetchError::Ssrf("10.0.0.1".into())));
        let mut denied = outcome(v5core::Class::Network, 0);
        denied.failures = vec!["denied: http://10.0.0.1/".into()];
        assert!(matches!(
            into_outcome(denied, RequestRoute::direct(), &refusal),
            Err(FetchError::Ssrf(_))
        ));
    }

    #[test]
    fn an_unsolicited_304_is_never_a_page() {
        assert!(map(outcome(v5core::Class::Content, 304)).is_err());
    }

    #[test]
    fn the_hop_guard_refuses_private_targets_and_stops_on_other_schemes() {
        let refusal: Arc<Mutex<Option<FetchError>>> = Arc::default();
        let guard = hop_guard(Arc::clone(&refusal), None);
        assert_eq!(
            guard("https://a.test/", "https://b.test/next"),
            v5core::Hop::Follow
        );
        assert!(matches!(
            guard("https://a.test/", "http://10.0.0.1/"),
            v5core::Hop::Deny(_)
        ));
        assert!(refusal.lock().unwrap().is_some());
        assert_eq!(
            guard("https://a.test/", "mailto:x@a.test"),
            v5core::Hop::Stop
        );
    }

    #[test]
    fn a_link_carries_its_referer_and_fetch_site() {
        let headers = navigation_headers(Some("https://a.test/page?q=1"), "https://b.test/x");
        assert_eq!(
            headers,
            vec![
                ("referer".to_string(), "https://a.test/".to_string()),
                ("sec-fetch-site".to_string(), "cross-site".to_string()),
            ]
        );
        assert!(navigation_headers(None, "https://b.test/").is_empty());
    }
}
