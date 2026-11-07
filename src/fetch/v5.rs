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

tokio::task_local! {
    /// The instant the caller stops waiting (an agent's `deadline_ms`), so the
    /// encounter plans inside it: it starts no attempt that cannot finish and
    /// ends on the best answer it has, instead of being cut off mid-search.
    pub static CALLER_DEADLINE: Instant;
}

/// What is left for one document under the caller's deadline, keeping a slice
/// for the reading that follows the fetch (extraction, PDF parsing).
fn document_budget() -> Duration {
    let Ok(until) = CALLER_DEADLINE.try_with(|until| *until) else {
        return DOCUMENT_DEADLINE;
    };
    let left = until.saturating_duration_since(Instant::now());
    left.saturating_sub((left / 5).min(Duration::from_millis(600)))
        .min(DOCUMENT_DEADLINE)
}

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
        add_configured_roots();
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
        // The proxy is read before the URL guard looks the target's name
        // up: a read that cannot leave through its proxy resolves nothing.
        let proxy = engine_proxy(document.proxy.as_ref())?;
        crate::fetch::guards::ensure_url_safe(document.url).await?;
        let refusal: Arc<Mutex<Option<FetchError>>> = Arc::default();
        let mut request = v5core::Request::new(document.url);
        request.proxy = proxy;
        request.learn = document.learn;
        request.cache = document.learn && !document.skip_cache;
        request.deadline = document_budget();
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
        // Read before the URL guard's lookup, as in `document`.
        let proxy = engine_proxy(shot.proxy)?;
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
            proxy,
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

/// The owner's `[tls] cert_file` and `cert_dir` (PEM bundles; a directory's
/// `.pem`, `.crt` and `.cer` files), trusted on top of the bundled and OS roots.
/// Once per process, before the first client is built.
fn add_configured_roots() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let tls = &crate::config::cfg().tls;
        for pem in configured_roots(&tls.cert_file, &tls.cert_dir) {
            v5core::add_trust_pem(pem);
        }
    });
}

fn configured_roots(file: &str, dir: &str) -> Vec<Vec<u8>> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if !file.is_empty() {
        paths.push(file.into());
    }
    if !dir.is_empty()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let mut found: Vec<std::path::PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        matches!(ext.to_ascii_lowercase().as_str(), "pem" | "crt" | "cer")
                    })
            })
            .collect();
        found.sort();
        paths.extend(found);
    }
    paths
        .into_iter()
        .filter_map(|path| match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                crate::diag!("[tls] cannot read {}: {error}", path.display());
                None
            }
        })
        .collect()
}

/// The URL v5 is handed for `proxy`. With none, v5 takes the proxy the
/// environment names (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`). A proxy URL
/// v5 cannot read, either way, is a proxy configuration error naming the
/// proxy, credentials masked (and its variable, for the environment), and
/// nothing is sent.
fn engine_proxy(proxy: Option<&Proxy>) -> Result<Option<String>, FetchError> {
    let Some(proxy) = proxy else {
        v5core::environment_proxies().map_err(FetchError::ProxyConfig)?;
        return Ok(None);
    };
    let url = proxy.wire_url();
    v5core::proxy_url(&url).map_err(FetchError::ProxyConfig)?;
    Ok(Some(url))
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
            // lane health reads; anything else is a dead lane. v5 leads a
            // refused credential with its status, so a port that happens to
            // contain 407 is not read as one.
            let lower = detail.to_ascii_lowercase();
            if lower.starts_with("407") || lower.contains("authentication required") {
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
        // Both see a wall; v5 may know whose it is (Akamai names its refusal
        // only by its reference link).
        (class, Verdict::Challenge(Vendor::Generic))
            if is_wall(class) && vendor(wall_vendor) != Vendor::Generic =>
        {
            Verdict::Challenge(vendor(wall_vendor))
        }
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
    use crate::fetch::client::Fetcher;
    use crate::profile::BrowserProfile;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

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
    fn v5_names_the_vendor_of_a_wall_both_saw() {
        // Akamai's 200 refusal: the product's detector sees a generic wall, v5
        // read the reference link.
        assert_eq!(
            verdict_for(
                v5core::Class::WallIdentity,
                Some("akamai"),
                Verdict::Challenge(Vendor::Generic)
            ),
            Verdict::Challenge(Vendor::Akamai)
        );
        // A vendor the product named itself is kept.
        assert_eq!(
            verdict_for(
                v5core::Class::WallIdentity,
                Some("akamai"),
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

    #[tokio::test]
    async fn the_callers_deadline_bounds_the_encounter() {
        assert_eq!(document_budget(), DOCUMENT_DEADLINE);
        let soon = Instant::now() + Duration::from_secs(5);
        let budget = CALLER_DEADLINE
            .scope(soon, async { document_budget() })
            .await;
        // Five seconds left, less the 600 ms kept for reading what comes back.
        assert!(
            budget <= Duration::from_millis(4400) && budget > Duration::from_millis(4300),
            "{budget:?}"
        );
        let later = Instant::now() + Duration::from_secs(120);
        let budget = CALLER_DEADLINE
            .scope(later, async { document_budget() })
            .await;
        assert_eq!(budget, DOCUMENT_DEADLINE);
        let past = Instant::now();
        let budget = CALLER_DEADLINE
            .scope(past, async { document_budget() })
            .await;
        assert_eq!(budget, Duration::ZERO);
    }

    #[test]
    fn configured_roots_read_the_file_and_the_directorys_certificates() {
        let dir = std::env::temp_dir().join(format!("donsetch-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("corp.pem");
        std::fs::write(&file, b"FILE").unwrap();
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        std::fs::write(bundle.join("b.crt"), b"B").unwrap();
        std::fs::write(bundle.join("a.PEM"), b"A").unwrap();
        std::fs::write(bundle.join("notes.txt"), b"no").unwrap();
        let roots = configured_roots(file.to_str().unwrap(), bundle.to_str().unwrap());
        assert_eq!(roots, vec![b"FILE".to_vec(), b"A".to_vec(), b"B".to_vec()]);
        assert!(configured_roots("", "").is_empty());
        // A missing file is named and skipped, never fatal.
        assert!(configured_roots("/nonexistent/ca.pem", "").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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

    // ── A proxy URL v5 cannot read ─────────────────────────────────────
    //
    // These tests install the process config (no environment proxy; loopback
    // targets allowed unless the test says otherwise), which can happen once
    // per process: they depend on nextest's one process per test.

    type Log = Arc<Mutex<Vec<String>>>;

    fn loopback_config() {
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
    }

    async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Option<String> {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            if bytes.len() > 16384 {
                return None;
            }
            bytes.push(stream.read_u8().await.ok()?);
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn page_reply(title: &str) -> Vec<u8> {
        let page = format!(
            "<!doctype html><html><head><title>{title}</title></head><body><article><h1>{title}</h1><p>{}</p></article></body></html>",
            "The request reached the page it asked for, and this paragraph is that page's content. ".repeat(4)
        );
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        )
        .into_bytes()
    }

    /// A plain-HTTP server on 127.0.0.1 that logs every request head and
    /// answers each one with a page titled `title`. As a proxy it answers an
    /// absolute-form request for any origin itself.
    async fn http_server(title: &'static str) -> (u16, Log, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Log = Arc::default();
        let seen = Arc::clone(&log);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let head = read_head(&mut stream).await?;
                    seen.lock().unwrap().push(head);
                    stream.write_all(&page_reply(title)).await.ok()?;
                    stream.shutdown().await.ok()
                });
            }
        });
        (port, log, task)
    }

    /// A SOCKS5 proxy on 127.0.0.1 that logs the `user:pass` each connection
    /// authenticates with (RFC 1929; empty when none is offered), grants the
    /// CONNECT and answers the tunnelled request itself.
    async fn socks5_server() -> (u16, Log, tokio::task::JoinHandle<()>) {
        async fn field(stream: &mut tokio::net::TcpStream) -> Option<String> {
            let len = stream.read_u8().await.ok()?;
            let mut bytes = vec![0u8; usize::from(len)];
            stream.read_exact(&mut bytes).await.ok()?;
            String::from_utf8(bytes).ok()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Log = Arc::default();
        let seen = Arc::clone(&log);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut greeting = [0u8; 2];
                    stream.read_exact(&mut greeting).await.ok()?;
                    let mut methods = vec![0u8; usize::from(greeting[1])];
                    stream.read_exact(&mut methods).await.ok()?;
                    let credentials = if methods.contains(&0x02) {
                        stream.write_all(&[0x05, 0x02]).await.ok()?;
                        let _version = stream.read_u8().await.ok()?;
                        let user = field(&mut stream).await?;
                        let pass = field(&mut stream).await?;
                        stream.write_all(&[0x01, 0x00]).await.ok()?;
                        format!("{user}:{pass}")
                    } else {
                        stream.write_all(&[0x05, 0x00]).await.ok()?;
                        String::new()
                    };
                    seen.lock().unwrap().push(credentials);
                    let mut request = [0u8; 4];
                    stream.read_exact(&mut request).await.ok()?;
                    let address = match request[3] {
                        0x01 => 4,
                        0x04 => 16,
                        _ => usize::from(stream.read_u8().await.ok()?),
                    };
                    let mut rest = vec![0u8; address + 2];
                    stream.read_exact(&mut rest).await.ok()?;
                    stream
                        .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
                        .await
                        .ok()?;
                    read_head(&mut stream).await?;
                    stream
                        .write_all(&page_reply("Through the proxy"))
                        .await
                        .ok()?;
                    stream.shutdown().await.ok()
                });
            }
        });
        (port, log, task)
    }

    /// A document read and a one-shot read of `url` through `proxy`: each
    /// one's status, or its error.
    async fn both_paths(proxy: &Proxy, url: &str) -> [Result<u16, FetchError>; 2] {
        let fetcher = Fetcher::new(BrowserProfile::host_default()).unwrap();
        let document = fetcher.fetch_via(url, Some(proxy)).await;
        let once = fetcher
            .fetch_once_via(url, &[], Some(proxy), false, None)
            .await;
        [document, once].map(|result| result.map(|out| out.status))
    }

    /// A SOCKS5 proxy whose password holds a raw '/', '#' or '?' parses, but
    /// the URL v5 is handed for it ends its address inside the password. The
    /// fetch must fail, say which proxy it could not read without echoing the
    /// credentials, and never reach the origin from this machine's own address.
    #[tokio::test]
    async fn a_proxy_url_v5_cannot_read_fails_the_fetch_and_never_goes_direct() {
        loopback_config();
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (port, proxied, proxy_task) = socks5_server().await;
        let url = format!("http://127.0.0.1:{origin_port}/page");
        for password in ["pa/ss", "pa#ss", "pa?ss"] {
            let proxy =
                Proxy::parse(&format!("socks5://alice:{password}@127.0.0.1:{port}")).unwrap();
            for result in both_paths(&proxy, &url).await {
                let report = format!(
                    "{password}: {result:?}; the origin took {} direct request(s), the proxy {} connection(s)",
                    origin.lock().unwrap().len(),
                    proxied.lock().unwrap().len()
                );
                let Err(FetchError::ProxyConfig(message)) = &result else {
                    panic!("{report}");
                };
                assert!(
                    message.contains(&format!("\"socks5h://***@127.0.0.1:{port}\"")),
                    "the error names the proxy: {report}"
                );
                assert!(
                    !message.contains("alice") && !message.contains(password),
                    "the error echoes the credentials: {report}"
                );
            }
        }
        origin_task.abort();
        proxy_task.abort();
        assert!(origin.lock().unwrap().is_empty(), "{:?}", origin.lock());
        assert!(proxied.lock().unwrap().is_empty(), "{:?}", proxied.lock());
    }

    /// With private egress not allowed the URL guard looks the target's name
    /// up on this machine. A read whose proxy URL v5 cannot read fails before
    /// that lookup; a read through a readable proxy still goes through the
    /// guard's lookup.
    #[tokio::test]
    async fn a_proxy_url_v5_cannot_read_is_refused_before_the_target_name_is_looked_up() {
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = false;
        crate::config::install(config).unwrap();
        let url = "http://no-such-host-for-donsetch.invalid/page";
        let lookups = || crate::egress::dns::stats().1;
        let before = lookups();
        let proxy = Proxy::parse("socks5://alice:pa/ss@127.0.0.1:9").unwrap();
        let results = both_paths(&proxy, url).await;
        assert!(
            results
                .iter()
                .all(|result| matches!(result, Err(FetchError::ProxyConfig(_))))
                && lookups() == before,
            "{results:?}; the target's name was looked up {} time(s)",
            lookups() - before
        );
        // Negative: a readable proxy still goes through the guard's lookup.
        let proxy = Proxy::parse("socks5://alice:s3cret@127.0.0.1:9").unwrap();
        let results = both_paths(&proxy, url).await;
        assert!(
            results
                .iter()
                .all(|result| result.is_err() && !matches!(result, Err(FetchError::ProxyConfig(_))))
                && lookups() > before,
            "{results:?}; {} lookup(s)",
            lookups() - before
        );
    }

    // Negative: a proxy URL v5 reads (SOCKS5 with and without credentials,
    // a percent-encoded '/', and an HTTP proxy) carries the fetch as before.
    #[tokio::test]
    async fn a_readable_socks5_or_http_proxy_still_carries_the_fetch() {
        loopback_config();
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (socks_port, socks, socks_task) = socks5_server().await;
        let (http_port, http, http_task) = http_server("Through the proxy").await;
        let url = format!("http://127.0.0.1:{origin_port}/page");
        for (userinfo, expected) in [
            ("alice:s3cret@", "alice:s3cret"),
            ("alice:pa%2Fss@", "alice:pa/ss"),
            ("", ""),
        ] {
            let proxy =
                Proxy::parse(&format!("socks5://{userinfo}127.0.0.1:{socks_port}")).unwrap();
            let results = both_paths(&proxy, &url).await;
            let seen = std::mem::take(&mut *socks.lock().unwrap());
            assert!(
                results.iter().all(|result| matches!(result, Ok(200)))
                    && !seen.is_empty()
                    && seen.iter().all(|credentials| credentials == expected),
                "socks5 {userinfo}: {results:?}; the proxy saw {seen:?}"
            );
        }
        let proxy = Proxy::parse(&format!("http://alice:s3cret@127.0.0.1:{http_port}")).unwrap();
        let results = both_paths(&proxy, &url).await;
        let heads = http.lock().unwrap().clone();
        let target = format!("GET http://127.0.0.1:{origin_port}/");
        let credentials = format!(
            "proxy-authorization: basic {}",
            crate::egress::proxy::base64("alice:s3cret").to_ascii_lowercase()
        );
        assert!(
            results.iter().all(|result| matches!(result, Ok(200)))
                && !heads.is_empty()
                && heads.iter().all(|head| head.starts_with(&target)
                    && head.to_ascii_lowercase().contains(&credentials)),
            "http: {results:?}; the proxy saw {heads:?}"
        );
        origin_task.abort();
        socks_task.abort();
        http_task.abort();
        assert!(origin.lock().unwrap().is_empty(), "{:?}", origin.lock());
    }

    // ── A read handed no proxy ─────────────────────────────────────────
    //
    // v5's transport then takes a proxy from the environment itself. These
    // tests set the process environment and install the process config:
    // they depend on nextest's one process per test.

    /// Environment proxies on (the default); loopback targets allowed.
    fn environment_config() {
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = true;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
    }

    /// The process environment names `HTTP_PROXY=value` and no other proxy.
    fn set_http_proxy(value: &str) {
        // SAFETY: test-only mutation of the process env; nextest runs each
        // test in its own process.
        unsafe {
            for name in [
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
                "REQUEST_METHOD",
            ] {
                std::env::remove_var(name);
            }
            std::env::set_var("HTTP_PROXY", value);
        }
    }

    async fn once_without_a_lane(url: &str) -> Result<u16, FetchError> {
        let fetcher = Fetcher::new(BrowserProfile::host_default()).unwrap();
        fetcher
            .fetch_once_via(url, &[], None, false, None)
            .await
            .map(|out| out.status)
    }

    /// A one-shot read handed no proxy (a first-wave search vertical,
    /// `donsetch update`) whose environment proxy the transport cannot read
    /// must fail as a proxy configuration error that names the variable and
    /// the proxy without its credentials, and reach neither the origin nor
    /// the proxy.
    #[tokio::test]
    async fn an_environment_proxy_v5_cannot_read_fails_a_read_without_a_lane() {
        environment_config();
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (port, proxied, proxy_task) = socks5_server().await;
        let url = format!("http://127.0.0.1:{origin_port}/page");
        for (value, named) in [
            // A raw '/' in the password ends the address early.
            (
                format!("socks5://alice:pa/ss@127.0.0.1:{port}"),
                format!("HTTP_PROXY: cannot read proxy URL \"socks5://***@127.0.0.1:{port}\": "),
            ),
            // The url crate reads these two; the transport does not.
            (
                format!("SOCKS5://alice:s3cret@127.0.0.1:{port}"),
                format!("HTTP_PROXY: cannot read proxy URL \"SOCKS5://***@127.0.0.1:{port}\": "),
            ),
            (
                format!("socks5://127.0.0.1:{port} "),
                format!("HTTP_PROXY: cannot read proxy URL \"socks5://127.0.0.1:{port} \": "),
            ),
        ] {
            set_http_proxy(&value);
            let result = once_without_a_lane(&url).await;
            let report = format!(
                "{value:?}: {result:?}; the origin took {} direct request(s), the proxy {} connection(s)",
                origin.lock().unwrap().len(),
                proxied.lock().unwrap().len()
            );
            let Err(FetchError::ProxyConfig(message)) = &result else {
                panic!("{report}");
            };
            assert!(
                message.starts_with(&named),
                "the error names the variable and the proxy: {report}"
            );
            assert!(
                ["alice", "pa/ss", "s3cret"]
                    .iter()
                    .all(|secret| !message.contains(secret)),
                "the error echoes the credentials: {report}"
            );
        }
        origin_task.abort();
        proxy_task.abort();
        assert!(origin.lock().unwrap().is_empty(), "{:?}", origin.lock());
        assert!(proxied.lock().unwrap().is_empty(), "{:?}", proxied.lock());
    }

    // Negative: an environment proxy the transport reads still carries a read
    // handed no proxy; a value without a scheme is an HTTP proxy.
    #[tokio::test]
    async fn a_readable_environment_proxy_still_carries_a_read_without_a_lane() {
        environment_config();
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (socks_port, socks, socks_task) = socks5_server().await;
        let (http_port, http, http_task) = http_server("Through the proxy").await;
        let url = format!("http://127.0.0.1:{origin_port}/page");
        set_http_proxy(&format!("socks5://alice:s3cret@127.0.0.1:{socks_port}"));
        let result = once_without_a_lane(&url).await;
        let seen = socks.lock().unwrap().clone();
        assert!(
            matches!(result, Ok(200))
                && !seen.is_empty()
                && seen.iter().all(|credentials| credentials == "alice:s3cret"),
            "socks5: {result:?}; the proxy saw {seen:?}"
        );
        set_http_proxy(&format!("127.0.0.1:{http_port}"));
        let result = once_without_a_lane(&url).await;
        let heads = http.lock().unwrap().clone();
        let target = format!("GET http://127.0.0.1:{origin_port}/");
        assert!(
            matches!(result, Ok(200))
                && !heads.is_empty()
                && heads.iter().all(|head| head.starts_with(&target)),
            "http: {result:?}; the proxy saw {heads:?}"
        );
        origin_task.abort();
        socks_task.abort();
        http_task.abort();
        assert!(origin.lock().unwrap().is_empty(), "{:?}", origin.lock());
    }
}
