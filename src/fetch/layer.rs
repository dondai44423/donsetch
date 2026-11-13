//! The product's fetches over the v5 core (`donsetch_fetch::Session`).
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

/// The longest deadline a caller can set (`deadline_ms` is clamped to it). A
/// caller horizon past it means the caller set none.
const CALLER_DEADLINE_MAX: Duration = Duration::from_secs(600);

/// What is left for one document under the caller's deadline, keeping a slice
/// for the reading that follows the fetch (extraction, PDF parsing). Without a
/// caller deadline it is [`DOCUMENT_DEADLINE`]; a caller who set one gave the
/// document that long, so a slow transfer can finish inside it.
fn document_budget() -> Duration {
    let Ok(until) = CALLER_DEADLINE.try_with(|until| *until) else {
        return DOCUMENT_DEADLINE;
    };
    let left = until.saturating_duration_since(Instant::now());
    let left = left.saturating_sub((left / 5).min(Duration::from_millis(600)));
    if left > CALLER_DEADLINE_MAX {
        DOCUMENT_DEADLINE
    } else {
        left
    }
}

/// How long an AWS pricing page waits for its price files, and the largest one
/// it reads: a page that cannot have them in this time is served as it came.
const PRICE_FILES_BUDGET: Duration = Duration::from_secs(8);
const PRICE_FILE_MAX: usize = 16 * 1024 * 1024;

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

tokio::task_local! {
    /// Caller-owned accounting for HTTP exchanges, including redirects.
    pub(crate) static REQUEST_COUNT: Arc<std::sync::atomic::AtomicUsize>;
    pub(crate) static REQUEST_LIMIT: usize;
    pub(crate) static REQUEST_SOURCE_COUNT: Arc<std::sync::atomic::AtomicUsize>;
    pub(crate) static REQUEST_ORIGIN_GUARD: OriginGuard;
    pub(crate) static REQUEST_RESPONSE: donsetch_fetch::ResponseHead;
}

pub(crate) type OriginGuard = Arc<dyn Fn(&str) -> Option<u64> + Send + Sync>;

fn request_start(refusal: Arc<Mutex<Option<FetchError>>>) -> Option<donsetch_fetch::RequestStart> {
    let total = REQUEST_COUNT.try_with(Arc::clone).ok();
    let source = REQUEST_SOURCE_COUNT.try_with(Arc::clone).ok();
    let limit = REQUEST_LIMIT.try_with(|limit| *limit).unwrap_or(usize::MAX);
    let guard = REQUEST_ORIGIN_GUARD.try_with(Arc::clone).ok();
    if total.is_none() && source.is_none() && guard.is_none() {
        return None;
    }
    Some(Arc::new(move |url: &str| {
        use std::sync::atomic::Ordering;
        if let Some(wait) = guard.as_ref().and_then(|guard| guard(url)) {
            *refusal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(FetchError::Http(format!("rested:{wait}")));
            return false;
        }
        if let Some(total) = &total
            && total
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                    (used < limit).then(|| used + 1)
                })
                .is_err()
        {
            return false;
        }
        if let Some(source) = &source {
            source.fetch_add(1, Ordering::Relaxed);
        }
        true
    }))
}

pub struct LayerFetcher {
    session: Arc<donsetch_fetch::Session>,
}

/// A document request as the product describes it.
pub struct Document<'a> {
    pub url: &'a str,
    /// The proxy `route` names for `url`, kept for every redirect hop; `None`
    /// is a direct read, which refuses a hop `route` sends through a proxy.
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
    /// The lane. `None` is the configured route, as for a document read
    /// without a lane ([`RequestRoute::configured`]): the proxy it names for
    /// `url` is kept for every redirect hop, and a read it sends direct
    /// refuses a hop it sends through a proxy.
    pub proxy: Option<&'a Proxy>,
    pub use_jar: bool,
    pub referer: Option<&'a str>,
    pub class: RequestClass,
    /// A request-local legacy client, by its User-Agent.
    pub user_agent: Option<&'a str>,
    /// An internal measured portfolio identity for a single challenge retry.
    pub identity: Option<&'static str>,
}

impl LayerFetcher {
    /// `persist` keeps v5's learned state (scores, cookies, credentials) and its
    /// page cache on disk under the product's cache root (`v5/`), so
    /// `DONSETCH_CACHE_DIR` and `[paths] cache_dir` move them with everything else.
    pub fn new(persist: bool) -> Self {
        add_configured_roots();
        let session = if persist {
            donsetch_fetch::Session::at(state_path())
        } else {
            donsetch_fetch::Session::new(false)
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
        crate::fetch::guards::ensure_url_safe_via(document.url, document.proxy.as_ref()).await?;
        let direct = document.proxy.is_none().then(|| document.route.clone());
        let refusal: Arc<Mutex<Option<FetchError>>> = Arc::default();
        let mut request = donsetch_fetch::Request::new(document.url);
        request.proxy = proxy;
        request.learn = document.learn;
        request.cache = document.learn && !document.skip_cache;
        request.deadline = document_budget();
        let cancel = donsetch_fetch::Cancel::new();
        request.policy = donsetch_fetch::Policy {
            hop: Some(hop_guard(Arc::clone(&refusal), document.gate, direct)),
            request_start: request_start(Arc::clone(&refusal)),
            response_head: REQUEST_RESPONSE.try_with(Arc::clone).ok(),
            navigation: navigation_headers(document.referer, document.url),
            cancel: Some(cancel.clone()),
            ..donsetch_fetch::Policy::default()
        };
        let started = Instant::now();
        let session = Arc::clone(&self.session);
        let outcome = blocking(cancel, move || session.fetch(&request)).await;
        let mut out = into_outcome(outcome, document.route, &refusal)?;
        self.fill_price_tables(&mut out, document.proxy.as_ref())
            .await;
        out.elapsed = started.elapsed();
        Ok(out)
    }

    /// An AWS pricing page's tables are templates its own script fills from a
    /// public price file; read the file the page names and put the filled
    /// tables in the page (see [`crate::fetch::pricing`]). A page that is not
    /// one, or a file that does not answer in time, leaves the body as served.
    async fn fill_price_tables(&self, out: &mut FetchOutcome, proxy: Option<&Proxy>) {
        if out.status != 200 || memchr::memmem::find(&out.body, b"data-pricing-markup=").is_none() {
            return;
        }
        let Ok(html) = std::str::from_utf8(&out.body) else {
            return;
        };
        let Some(plan) = crate::fetch::pricing::plan(html, &out.url) else {
            return;
        };
        let referer = out.url.clone();
        let reads = plan.files.iter().map(|(name, url)| {
            let origin = [("origin".to_string(), plan.origin.clone())];
            let referer = referer.as_str();
            async move {
                let reply = self
                    .once(OneShot {
                        url,
                        conditional: &origin,
                        proxy,
                        use_jar: false,
                        referer: Some(referer),
                        class: RequestClass::Fetch,
                        user_agent: None,
                        identity: None,
                    })
                    .await
                    .ok()
                    .filter(|r| r.status == 200 && r.body.len() <= PRICE_FILE_MAX)?;
                let json = serde_json::from_slice(&reply.body).ok()?;
                Some((name.clone(), json))
            }
        });
        let Ok(files) =
            tokio::time::timeout(PRICE_FILES_BUDGET, futures_util::future::join_all(reads)).await
        else {
            return;
        };
        let data: std::collections::HashMap<String, serde_json::Value> =
            files.into_iter().flatten().collect();
        let mut body =
            crate::fetch::pricing::apply(html, &out.url, &data).unwrap_or_else(|| html.to_string());
        // The EC2 on-demand page's instance grid is not one of its templates:
        // its iframe app builds the table at run time from its own price file.
        // Read that file for the selection the app opens with and put the rows
        // where the iframe sits (see [`crate::fetch::pricing::grid`]).
        if let Some((at, url, region)) = crate::fetch::pricing::grid_plan(&body, &out.url) {
            let origin = [(
                "origin".to_string(),
                format!("https://{}", crate::fetch::pricing::PAGE_HOST),
            )];
            let referer = out.url.as_str();
            let reply = tokio::time::timeout(
                PRICE_FILES_BUDGET,
                self.once(OneShot {
                    url: &url,
                    conditional: &origin,
                    proxy,
                    use_jar: false,
                    referer: Some(referer),
                    class: RequestClass::Fetch,
                    user_agent: None,
                    identity: None,
                }),
            )
            .await;
            if let Ok(Ok(reply)) = reply
                && reply.status == 200
                && reply.body.len() <= PRICE_FILE_MAX
                && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&reply.body)
                && let Some(table) = crate::fetch::pricing::grid_table(&json, region)
            {
                body.insert_str(at, &table);
            }
        }
        if body != html {
            out.body = body.into_bytes();
        }
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
        let (route, lane) = match shot.proxy {
            Some(proxy) => (RequestRoute::pinned(proxy.clone()), Some(proxy.clone())),
            None => {
                // No lane: the route a document read takes without one
                // (Fetcher::document), so [proxy], proxy.from_environment and
                // the bypass lists decide here too, and the URL's own checks
                // come first as they do there. v5 is handed no proxy only for
                // a direct read: it reads none from the environment itself.
                crate::fetch::guards::validate_url_basic(shot.url)?;
                let route = RequestRoute::configured();
                let lane = route.proxy_for(shot.url)?;
                (route, lane)
            }
        };
        // Read before the URL guard's lookup, as in `document`.
        let proxy = engine_proxy(lane.as_ref())?;
        crate::fetch::guards::ensure_url_safe_via(shot.url, lane.as_ref()).await?;
        let direct = lane.is_none().then(|| route.clone());
        let mut headers: Vec<(String, String)> = Vec::new();
        let kind = if shot.class == RequestClass::Navigation {
            donsetch_fetch::Kind::Navigate
        } else {
            let (accept, dest) = shot.class.accept_and_dest();
            headers.push(("accept".into(), accept));
            headers.push((
                "sec-fetch-mode".into(),
                if shot.class.is_cors() {
                    "cors"
                } else {
                    "no-cors"
                }
                .into(),
            ));
            headers.push(("sec-fetch-dest".into(), dest));
            donsetch_fetch::Kind::Subresource
        };
        if let Some(agent) = shot.user_agent {
            headers.push(("user-agent".into(), agent.to_string()));
        }
        match shot.referer {
            Some(referer) => headers.extend(navigation_headers(Some(referer), shot.url)),
            // A subresource always has an initiator; with none named it is the
            // page's own origin.
            None if kind == donsetch_fetch::Kind::Subresource => {
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
        let cancel = donsetch_fetch::Cancel::new();
        let once = donsetch_fetch::Once {
            identity: Some(
                if shot.user_agent.is_some() {
                    LEGACY_IDENTITY
                } else {
                    shot.identity.unwrap_or(ONCE_IDENTITY)
                }
                .to_string(),
            ),
            proxy,
            kind,
            headers,
            cookies: shot.use_jar,
            timeout: ONCE_TIMEOUT,
            cap: REQUEST_COUNT.try_with(|_| 3 << 20).ok(),
            policy: donsetch_fetch::Policy {
                hop: Some(hop_guard(Arc::clone(&refusal), None, direct)),
                request_start: request_start(Arc::clone(&refusal)),
                response_head: REQUEST_RESPONSE.try_with(Arc::clone).ok(),
                request_max: Duration::MAX,
                cancel: Some(cancel.clone()),
                ..donsetch_fetch::Policy::default()
            },
            ..donsetch_fetch::Once::new(shot.url)
        };
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
        let mut names: Vec<String> = donsetch_fetch::identity::names()
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
            donsetch_fetch::add_trust_pem(pem);
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

/// The URL v5 is handed for `proxy`; with none the read is direct. A proxy
/// URL v5 cannot read is a proxy configuration error naming the proxy, its
/// user name and password masked, and nothing is sent.
fn engine_proxy(proxy: Option<&Proxy>) -> Result<Option<String>, FetchError> {
    let Some(proxy) = proxy else {
        return Ok(None);
    };
    let url = proxy.wire_url();
    donsetch_fetch::proxy_url(&url).map_err(FetchError::ProxyConfig)?;
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
async fn blocking<F, R>(cancel: donsetch_fetch::Cancel, job: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    struct OnDrop(donsetch_fetch::Cancel);
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
/// then the caller's own gate (crawl scope and robots), then the route. Every
/// hop leaves through the egress of the first URL; `direct` is the route of a
/// read that started direct, and a hop that route sends through a proxy is
/// refused as a proxy configuration error instead of being dialed direct. A
/// refused hop records its typed error in `refusal`; a hop the gate declines
/// ends the chain on the 3xx, which the caller reads as the honest skip.
fn hop_guard(
    refusal: Arc<Mutex<Option<FetchError>>>,
    gate: Option<RedirectGate>,
    direct: Option<RequestRoute>,
) -> donsetch_fetch::HopGuard {
    let handle = tokio::runtime::Handle::try_current().ok();
    Arc::new(move |previous: &str, next: &str| {
        let refuse = |error: FetchError| {
            *refusal.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
            donsetch_fetch::Hop::Deny(next.to_string())
        };
        let Ok(base) = url::Url::parse(previous) else {
            return donsetch_fetch::Hop::Deny(next.to_string());
        };
        let url = match crate::fetch::guards::validate_redirect_url(&base, next) {
            Ok(url) => url,
            Err(FetchError::NonHttpRedirect(_)) => return donsetch_fetch::Hop::Stop,
            Err(error) => return refuse(error),
        };
        // The session runs on a blocking thread, outside the runtime: waiting
        // on the async gate here is allowed and parks only it.
        if let (Some(gate), Some(handle)) = (&gate, &handle)
            && !handle.block_on(gate(url.clone()))
        {
            return donsetch_fetch::Hop::Stop;
        }
        let Some(route) = &direct else {
            return donsetch_fetch::Hop::Follow;
        };
        // The chain's client has no proxy, so this hop would leave from this
        // machine's own address; where the route names a proxy for it, the
        // read fails instead.
        match route.proxy_for(url.as_str()) {
            Ok(None) => donsetch_fetch::Hop::Follow,
            Ok(Some(_)) => refuse(FetchError::ProxyConfig(format!(
                "redirect to {} refused: the configured route sends it through a proxy, \
                 and this read started direct",
                url.origin().ascii_serialization()
            ))),
            Err(error) => refuse(error),
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
    if (raw.starts_with("denied:") || raw.starts_with("budget:"))
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
        "timeout" => match detail
            .strip_prefix("the transfer was still arriving after ")
            .and_then(|rest| rest.strip_suffix(" bytes"))
            .and_then(|bytes| bytes.parse().ok())
        {
            Some(received) => FetchError::SlowTransfer(received),
            None => FetchError::Timeout,
        },
        "tls" => FetchError::Tls(detail.to_owned()),
        "connect" => FetchError::Io(std::io::Error::other(detail.to_owned())),
        "proxy" => {
            // A proxy demanding credentials keeps the auth-fail shape the
            // lane health reads; a proxy that answered it could not reach the
            // target is marked so lane health leaves the lane alone; anything
            // else is a dead lane. v5 leads a refused credential with its
            // status, so a port that happens to contain 407 is not read as one.
            let lower = detail.to_ascii_lowercase();
            let message = format!("proxy: {detail}");
            if lower.starts_with("407") || lower.contains("authentication required") {
                FetchError::Http(format!("CONNECT -> 407 ({detail})"))
            } else if TARGET_REFUSALS.iter().any(|reply| lower.contains(reply)) {
                FetchError::Io(std::io::Error::other(TargetRefused(message)))
            } else {
                FetchError::Io(std::io::Error::other(message))
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

/// A proxy's answer that it took the request and could not reach the target,
/// in the lowercase words of the transport's error chain: the SOCKS5 replies
/// about the route to the target (RFC 1928: 0x03 network unreachable, 0x04
/// host unreachable, 0x05 connection refused, 0x06 TTL expired) as tokio-socks
/// names them, and a CONNECT answered with a status other than 200 and 407,
/// which wreq reports without the status. The other SOCKS5 replies (general
/// server failure, ruleset, command or address type not supported) are about
/// the proxy and are not listed.
const TARGET_REFUSALS: &[&str] = &[
    "error during socks handshake: network unreachable",
    "error during socks handshake: host unreachable",
    "error during socks handshake: connection refused",
    "error during socks handshake: ttl expired",
    "tunnel error: unsuccessful",
];

/// A proxy's refusal of the target (`TARGET_REFUSALS`), carried inside
/// `FetchError::Io` with the message it would have had without the mark.
#[derive(Debug)]
struct TargetRefused(String);

impl std::fmt::Display for TargetRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TargetRefused {}

/// True when `error` is a proxy's answer that it could not reach the target:
/// the lane connected and answered, so the failure is the target's, not the
/// lane's.
pub(crate) fn proxy_refused_the_target(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<TargetRefused>())
}

/// v5's outcome as the product's. The product's own wall detector reads the
/// body, as on every path; v5's wall class wins over a body-only `ContentOk`.
fn into_outcome(
    outcome: donsetch_fetch::Outcome,
    route: RequestRoute,
    refusal: &Mutex<Option<FetchError>>,
) -> Result<FetchOutcome, FetchError> {
    if outcome.class == donsetch_fetch::Class::Network && outcome.status == 0 {
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
        verified: outcome.class == donsetch_fetch::Class::Content && verdict == Verdict::ContentOk,
    })
}

/// v5's class decides bot walls: it read the answer against a contract and
/// knows which identity got through, while the product's markers also fire on
/// the passive vendor scripts real pages carry (Medium's content page ships
/// Cloudflare's). Login walls, paywalls and soft 404s stay the product's call.
fn verdict_for(
    class: donsetch_fetch::Class,
    wall_vendor: Option<&str>,
    detected: Verdict,
) -> Verdict {
    match (class, detected) {
        (donsetch_fetch::Class::Content, Verdict::Challenge(_) | Verdict::Blocked) => {
            Verdict::ContentOk
        }
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

fn is_wall(class: donsetch_fetch::Class) -> bool {
    use donsetch_fetch::Class::*;
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

    fn outcome(class: donsetch_fetch::Class, status: u16) -> donsetch_fetch::Outcome {
        donsetch_fetch::Outcome {
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

    fn map(outcome: donsetch_fetch::Outcome) -> Result<FetchOutcome, FetchError> {
        into_outcome(outcome, RequestRoute::direct(), &Mutex::new(None))
    }

    #[test]
    fn a_v5_wall_overrides_a_body_only_content_verdict() {
        let mut walled = outcome(donsetch_fetch::Class::WallProgram, 200);
        walled.wall = Some(donsetch_fetch::Wall {
            vendor: "cloudflare".into(),
            marker: "cf_chl_opt".into(),
            asks: "managed challenge (page program)",
        });
        assert_eq!(
            map(walled).expect("outcome").verdict,
            Verdict::Challenge(Vendor::Cloudflare)
        );
        let content = map(outcome(donsetch_fetch::Class::Content, 200)).expect("outcome");
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
        let content = donsetch_fetch::Class::Content;
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
                donsetch_fetch::Class::Incomplete,
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
                donsetch_fetch::Class::WallIdentity,
                Some("akamai"),
                Verdict::Challenge(Vendor::Generic)
            ),
            Verdict::Challenge(Vendor::Akamai)
        );
        // A vendor the product named itself is kept.
        assert_eq!(
            verdict_for(
                donsetch_fetch::Class::WallIdentity,
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
            let mut served = outcome(donsetch_fetch::Class::Content, 200);
            served.cache = state.map(str::to_string);
            assert_eq!(map(served).expect("outcome").cache, expected, "{state:?}");
        }
    }

    #[test]
    fn records_reach_the_product_as_text_not_the_shell_they_came_from() {
        let mut data = outcome(donsetch_fetch::Class::Content, 200);
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
            (
                "timeout: the transfer was still arriving after 1048576 bytes",
                "SlowTransfer(1048576)",
            ),
            ("dns: failed to lookup address", "Dns"),
            ("tls: certificate verify failed", "Tls"),
            ("connect: connection refused", "Io"),
            ("redirect: more than 10 redirects", "TooManyRedirects"),
            ("proxy: 407 Proxy Authentication Required", "Http"),
        ] {
            let mut failed = outcome(donsetch_fetch::Class::Network, 0);
            failed.failures = vec![failure.into()];
            let error = map(failed).err().expect("an error");
            assert!(
                format!("{error:?}").starts_with(expected),
                "{failure}: {error:?}"
            );
        }
        let mut proxy = outcome(donsetch_fetch::Class::Network, 0);
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
            map(outcome(donsetch_fetch::Class::Network, 0)),
            Err(FetchError::Timeout)
        ));
    }

    #[test]
    fn a_refused_hop_comes_back_as_the_typed_refusal() {
        let refusal = Mutex::new(Some(FetchError::Ssrf("10.0.0.1".into())));
        let mut denied = outcome(donsetch_fetch::Class::Network, 0);
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
        // A caller who gave two minutes gave them to the document: a slow
        // transfer may use them.
        let later = Instant::now() + Duration::from_secs(120);
        let budget = CALLER_DEADLINE
            .scope(later, async { document_budget() })
            .await;
        assert!(
            budget > Duration::from_secs(118) && budget <= Duration::from_millis(119_400),
            "{budget:?}"
        );
        // The horizon set when the caller gave no deadline keeps the
        // document's own budget.
        let none = Instant::now() + Duration::from_secs(86_400);
        let budget = CALLER_DEADLINE
            .scope(none, async { document_budget() })
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
        assert!(map(outcome(donsetch_fetch::Class::Content, 304)).is_err());
    }

    #[test]
    fn the_hop_guard_refuses_private_targets_and_stops_on_other_schemes() {
        let refusal: Arc<Mutex<Option<FetchError>>> = Arc::default();
        let guard = hop_guard(Arc::clone(&refusal), None, None);
        assert_eq!(
            guard("https://a.test/", "https://b.test/next"),
            donsetch_fetch::Hop::Follow
        );
        assert!(matches!(
            guard("https://a.test/", "http://10.0.0.1/"),
            donsetch_fetch::Hop::Deny(_)
        ));
        assert!(refusal.lock().unwrap().is_some());
        assert_eq!(
            guard("https://a.test/", "mailto:x@a.test"),
            donsetch_fetch::Hop::Stop
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
        reply_server(page_reply(title)).await
    }

    /// A plain-HTTP server on 127.0.0.1 that logs every request head and
    /// answers each one with `reply`, a CONNECT included.
    async fn reply_server(reply: Vec<u8>) -> (u16, Log, tokio::task::JoinHandle<()>) {
        let reply: Arc<[u8]> = reply.into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Log = Arc::default();
        let seen = Arc::clone(&log);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let seen = Arc::clone(&seen);
                let reply = Arc::clone(&reply);
                tokio::spawn(async move {
                    let head = read_head(&mut stream).await?;
                    seen.lock().unwrap().push(head);
                    stream.write_all(&reply).await.ok()?;
                    stream.shutdown().await.ok()
                });
            }
        });
        (port, log, task)
    }

    /// A listener on 127.0.0.1 that logs each connection it accepts and closes
    /// it unread: a TLS client gets no handshake from it.
    async fn counting_listener() -> (u16, Log, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Log = Arc::default();
        let seen = Arc::clone(&log);
        let task = tokio::spawn(async move {
            while let Ok((stream, peer)) = listener.accept().await {
                seen.lock().unwrap().push(peer.to_string());
                drop(stream);
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

    // nextest isolates the installed process configuration.
    #[tokio::test]
    async fn search_request_budget_counts_redirects_and_stops_before_an_extra_request() {
        loopback_config();
        let reply = b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (port, log, task) = reply_server(reply).await;
        let fetcher =
            crate::fetch::client::Fetcher::new(crate::profile::BrowserProfile::host_default())
                .unwrap();
        for limit in [2, 0] {
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let source = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let before = log.lock().unwrap().len();
            let result = REQUEST_COUNT
                .scope(
                    Arc::clone(&count),
                    REQUEST_LIMIT.scope(
                        limit,
                        REQUEST_SOURCE_COUNT.scope(
                            Arc::clone(&source),
                            fetcher.fetch_once_via(
                                &format!("http://127.0.0.1:{port}/start"),
                                &[],
                                None,
                                false,
                                None,
                            ),
                        ),
                    ),
                )
                .await;
            assert!(
                matches!(&result, Err(FetchError::Http(message)) if message.contains("budget: request ceiling reached")),
                "{:?}",
                result.err()
            );
            assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), limit);
            assert_eq!(source.load(std::sync::atomic::Ordering::Relaxed), limit);
            assert_eq!(log.lock().unwrap().len() - before, limit);
        }
        task.abort();
    }

    // nextest isolates the installed process configuration and cache root.
    #[tokio::test]
    async fn search_origin_rest_applies_to_other_paths_but_not_other_origins() {
        loopback_config();
        let (port, log, task) = reply_server(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 900\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()).await;
        let (other_port, other_log, other_task) = http_server("Another origin").await;
        let ledger = Arc::new(Mutex::new(crate::search::ledger::Ledger::load()));
        let (guard, observe) = crate::search::origin_policy(ledger);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetcher = Fetcher::new(BrowserProfile::host_default()).unwrap();
        REQUEST_COUNT.scope(Arc::clone(&count), REQUEST_ORIGIN_GUARD.scope(guard, REQUEST_RESPONSE.scope(observe, async {
            let url = format!("http://127.0.0.1:{port}/first");
            let first = fetcher.fetch_once_via(&url, &[], None, false, None).await.unwrap();
            assert_eq!(first.status, 429);
            let second = fetcher.fetch_once_via(&url.replace("/first", "/different"), &[], None, false, None).await;
            assert!(matches!(&second, Err(FetchError::Http(message)) if message.starts_with("rested:")), "{:?}", second.err());
            assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
            let other = fetcher.fetch_once_via(&format!("http://127.0.0.1:{other_port}/page"), &[], None, false, None).await.unwrap();
            assert_eq!(other.status, 200);
            assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 2);
        }))).await;
        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(other_log.lock().unwrap().len(), 1);
        task.abort();
        other_task.abort();
    }

    /// A SOCKS5 proxy whose password holds a '/', '#' or '?' is handed to v5
    /// with that password percent-encoded, so the read leaves through the
    /// proxy with the password as written, and never from this machine's own
    /// address.
    #[tokio::test]
    async fn a_proxy_password_with_reserved_characters_reaches_the_proxy_never_direct() {
        loopback_config();
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (port, proxied, proxy_task) = socks5_server().await;
        let url = format!("http://127.0.0.1:{origin_port}/page");
        for password in ["pa/ss", "pa#ss", "pa?ss"] {
            let proxy =
                Proxy::parse(&format!("socks5://alice:{password}@127.0.0.1:{port}")).unwrap();
            let before = proxied.lock().unwrap().len();
            let results = both_paths(&proxy, &url).await;
            let seen: Vec<String> = proxied.lock().unwrap()[before..].to_vec();
            assert!(
                results.iter().all(|result| matches!(result, Ok(200)))
                    && seen.len() >= 2
                    && seen
                        .iter()
                        .all(|credentials| *credentials == format!("alice:{password}")),
                "{password}: {results:?}; the proxy saw {seen:?}"
            );
        }
        origin_task.abort();
        proxy_task.abort();
        assert!(origin.lock().unwrap().is_empty(), "{:?}", origin.lock());
    }

    /// With private egress not allowed the URL guard looks a direct read's
    /// target name up on this machine. A SOCKS5 read leaves the name to the
    /// proxy, whatever its password holds: nothing is looked up here, and a
    /// proxy that cannot be reached is a network failure, not a configuration
    /// one.
    #[tokio::test]
    async fn a_proxied_read_leaves_the_target_name_to_the_proxy_whatever_its_password_holds() {
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = false;
        crate::config::install(config).unwrap();
        let url = "http://no-such-host-for-donsetch.invalid/page";
        let lookups = || crate::egress::dns::stats().1;
        let before = lookups();
        for password in ["pa/ss", "s3cret"] {
            let proxy = Proxy::parse(&format!("socks5://alice:{password}@127.0.0.1:9")).unwrap();
            let results = both_paths(&proxy, url).await;
            assert!(
                results
                    .iter()
                    .all(|result| result.is_err()
                        && !matches!(result, Err(FetchError::ProxyConfig(_))))
                    && lookups() == before,
                "{password}: {results:?}; the target's name was looked up {} time(s)",
                lookups() - before
            );
        }
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

    // ── A read given no lane ───────────────────────────────────────────
    //
    // A document read and a one-shot read given no lane take the configured
    // route. These tests set the process environment and install the
    // process config: they depend on nextest's one process per test.

    /// Loopback targets allowed; `edit` sets the rest.
    fn install_config(edit: impl FnOnce(&mut crate::config::DonsetchConfig)) {
        let mut config = crate::config::DonsetchConfig::default();
        config.fetch.allow_private_egress = true;
        edit(&mut config);
        crate::config::install(config).unwrap();
    }

    /// The process environment names exactly `vars` among the proxy
    /// variables.
    fn set_proxy_environment(vars: &[(&str, &str)]) {
        // SAFETY: test-only mutation of the process env; nextest runs each
        // test in its own process.
        unsafe {
            for name in [
                "HTTP_PROXY",
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
            for (name, value) in vars {
                std::env::set_var(name, value);
            }
        }
    }

    /// A document read and a one-shot read of `url`, neither given a lane:
    /// each one's status, or its error. Each read has its own fetcher, so no
    /// client is reused across reads, and the document read is a cold probe,
    /// so no page comes from the cache.
    async fn reads_without_a_lane(url: &str) -> [Result<u16, FetchError>; 2] {
        let document = Fetcher::new(BrowserProfile::host_default())
            .unwrap()
            .fetch_cold_probe(url)
            .await;
        let once = Fetcher::new(BrowserProfile::host_default())
            .unwrap()
            .fetch_once_via(url, &[], None, false, None)
            .await;
        [document, once].map(|result| result.map(|out| out.status))
    }

    /// With environment proxies turned off (`proxy.from_environment = false`,
    /// what `DONSETCH_NO_ENV_PROXY=1` sets), a readable proxy in the
    /// environment carries neither read: both reach the origin directly.
    #[tokio::test]
    async fn with_environment_proxies_off_neither_read_takes_the_environment_proxy() {
        install_config(|config| config.proxy.from_environment = false);
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (proxy_port, proxied, proxy_task) = http_server("Through the proxy").await;
        let proxy = format!("http://127.0.0.1:{proxy_port}");
        set_proxy_environment(&[
            ("HTTP_PROXY", &proxy),
            ("http_proxy", &proxy),
            ("ALL_PROXY", &proxy),
            ("all_proxy", &proxy),
        ]);
        let results = reads_without_a_lane(&format!("http://127.0.0.1:{origin_port}/page")).await;
        let (direct, through) = (origin.lock().unwrap().len(), proxied.lock().unwrap().len());
        origin_task.abort();
        proxy_task.abort();
        assert!(
            results.iter().all(|result| matches!(result, Ok(200))) && direct >= 2 && through == 0,
            "{results:?}; the origin took {direct} request(s), the proxy {through}"
        );
    }

    /// A host in `[proxy] no_proxy` is read directly on both reads while the
    /// environment names a readable proxy; a host the list does not name
    /// rides that proxy.
    #[tokio::test]
    async fn a_host_in_proxy_no_proxy_is_read_directly() {
        install_config(|config| {
            config.proxy.from_environment = true;
            config.proxy.no_proxy = "127.0.0.1".into();
        });
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (proxy_port, proxied, proxy_task) = http_server("Through the proxy").await;
        set_proxy_environment(&[("HTTP_PROXY", &format!("http://127.0.0.1:{proxy_port}"))]);
        // Negative: "localhost" is not in the list. The proxy answers for any
        // origin, so the name is never looked up.
        let other = format!("http://localhost:{origin_port}/page");
        let results = reads_without_a_lane(&other).await;
        let heads = std::mem::take(&mut *proxied.lock().unwrap());
        assert!(
            results.iter().all(|result| matches!(result, Ok(200)))
                && heads.len() >= 2
                && heads
                    .iter()
                    .all(|head| head.starts_with(&format!("GET {other} ")))
                && origin.lock().unwrap().is_empty(),
            "{results:?}; the proxy saw {heads:?}; the origin {:?}",
            origin.lock()
        );
        let results = reads_without_a_lane(&format!("http://127.0.0.1:{origin_port}/page")).await;
        let (direct, through) = (origin.lock().unwrap().len(), proxied.lock().unwrap().len());
        origin_task.abort();
        proxy_task.abort();
        assert!(
            results.iter().all(|result| matches!(result, Ok(200))) && direct >= 2 && through == 0,
            "{results:?}; the origin took {direct} request(s), the proxy {through}"
        );
    }

    /// `[proxy] all` carries a read given no lane, one-shot as well as
    /// document, with no proxy in the environment.
    #[tokio::test]
    async fn proxy_all_carries_a_read_without_a_lane() {
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (proxy_port, proxied, proxy_task) = http_server("Through the proxy").await;
        install_config(|config| {
            config.proxy.from_environment = false;
            config.proxy.all = format!("http://127.0.0.1:{proxy_port}");
        });
        set_proxy_environment(&[]);
        let url = format!("http://127.0.0.1:{origin_port}/page");
        let results = reads_without_a_lane(&url).await;
        let heads = proxied.lock().unwrap().clone();
        let direct = origin.lock().unwrap().len();
        origin_task.abort();
        proxy_task.abort();
        assert!(
            results.iter().all(|result| matches!(result, Ok(200)))
                && heads.len() >= 2
                && heads
                    .iter()
                    .all(|head| head.starts_with(&format!("GET {url} ")))
                && direct == 0,
            "{results:?}; the proxy saw {heads:?}; the origin took {direct} request(s)"
        );
    }

    /// A host `NO_PROXY` exempts is read directly on both reads, whatever the
    /// proxy it is exempted from: a proxy URL v5 cannot read is not read for
    /// it.
    #[tokio::test]
    async fn a_host_no_proxy_exempts_is_read_directly_past_a_proxy_v5_cannot_read() {
        install_config(|config| config.proxy.from_environment = true);
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (port, proxied, proxy_task) = socks5_server().await;
        set_proxy_environment(&[
            (
                "HTTP_PROXY",
                &format!("socks5://alice:pa/ss@127.0.0.1:{port}"),
            ),
            ("NO_PROXY", "127.0.0.1"),
        ]);
        let results = reads_without_a_lane(&format!("http://127.0.0.1:{origin_port}/page")).await;
        let (direct, through) = (origin.lock().unwrap().len(), proxied.lock().unwrap().len());
        origin_task.abort();
        proxy_task.abort();
        assert!(
            results.iter().all(|result| matches!(result, Ok(200))) && direct >= 2 && through == 0,
            "{results:?}; the origin took {direct} request(s), the proxy {through}"
        );
    }

    /// An environment proxy whose password holds a '/', '#' or '?' carries a
    /// read given no lane, document or one-shot, with the password as
    /// written, and the origin is never reached from this machine's own
    /// address.
    #[tokio::test]
    async fn an_environment_proxy_password_with_reserved_characters_carries_a_read_without_a_lane()
    {
        install_config(|config| config.proxy.from_environment = true);
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (port, proxied, proxy_task) = socks5_server().await;
        let url = format!("http://127.0.0.1:{origin_port}/page");
        for password in ["pa/ss", "pa#ss", "pa?ss"] {
            set_proxy_environment(&[(
                "HTTP_PROXY",
                &format!("socks5://alice:{password}@127.0.0.1:{port}"),
            )]);
            let before = proxied.lock().unwrap().len();
            let results = reads_without_a_lane(&url).await;
            let seen: Vec<String> = proxied.lock().unwrap()[before..].to_vec();
            assert!(
                results.iter().all(|result| matches!(result, Ok(200)))
                    && seen.len() >= 2
                    && seen
                        .iter()
                        .all(|credentials| *credentials == format!("alice:{password}")),
                "{password}: {results:?}; the proxy saw {seen:?}"
            );
        }
        origin_task.abort();
        proxy_task.abort();
        assert!(origin.lock().unwrap().is_empty(), "{:?}", origin.lock());
    }

    // Negative: an environment proxy v5 reads carries both reads given no
    // lane (a value without a scheme is an HTTP proxy), and a host NO_PROXY
    // exempts from it is still read directly.
    #[tokio::test]
    async fn a_readable_environment_proxy_still_carries_a_read_without_a_lane() {
        install_config(|config| config.proxy.from_environment = true);
        let (origin_port, origin, origin_task) = http_server("Direct from the origin").await;
        let (socks_port, socks, socks_task) = socks5_server().await;
        let (http_port, http, http_task) = http_server("Through the proxy").await;
        let url = format!("http://127.0.0.1:{origin_port}/page");
        set_proxy_environment(&[(
            "HTTP_PROXY",
            &format!("socks5://alice:s3cret@127.0.0.1:{socks_port}"),
        )]);
        let results = reads_without_a_lane(&url).await;
        let seen = socks.lock().unwrap().clone();
        assert!(
            results.iter().all(|result| matches!(result, Ok(200)))
                && seen.len() >= 2
                && seen.iter().all(|credentials| credentials == "alice:s3cret"),
            "socks5: {results:?}; the proxy saw {seen:?}"
        );
        set_proxy_environment(&[("HTTP_PROXY", &format!("127.0.0.1:{http_port}"))]);
        let results = reads_without_a_lane(&url).await;
        let heads = http.lock().unwrap().clone();
        assert!(
            results.iter().all(|result| matches!(result, Ok(200)))
                && heads.len() >= 2
                && heads
                    .iter()
                    .all(|head| head.starts_with(&format!("GET {url} "))),
            "http: {results:?}; the proxy saw {heads:?}"
        );
        assert!(origin.lock().unwrap().is_empty(), "{:?}", origin.lock());
        set_proxy_environment(&[
            ("HTTP_PROXY", &format!("http://127.0.0.1:{http_port}")),
            ("NO_PROXY", "127.0.0.1"),
        ]);
        let results = reads_without_a_lane(&url).await;
        let (direct, through) = (
            origin.lock().unwrap().len(),
            http.lock().unwrap().len() - heads.len(),
        );
        origin_task.abort();
        socks_task.abort();
        http_task.abort();
        assert!(
            results.iter().all(|result| matches!(result, Ok(200))) && direct >= 2 && through == 0,
            "NO_PROXY: {results:?}; the origin took {direct} request(s), the proxy {through}"
        );
    }

    /// A read that starts direct dials no redirect hop its route sends
    /// through a proxy. With `HTTPS_PROXY` alone, an `http://` page that
    /// redirects to `https://` fails both reads given no lane as a proxy
    /// configuration error that names the hop's origin, and the `https://`
    /// target gets no connection, direct or through the proxy. Negatives:
    /// with `HTTP_PROXY` set too, the proxy carries both hops; with no proxy
    /// anywhere, the redirect is followed direct.
    #[tokio::test]
    async fn a_read_that_starts_direct_dials_no_hop_its_route_proxies() {
        install_config(|config| config.proxy.from_environment = true);
        let (target_port, target, target_task) = counting_listener().await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: https://localhost:{target_port}/page\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        let (origin_port, origin, origin_task) = reply_server(redirect.clone()).await;
        // As a proxy it answers the first hop with the same redirect, and the
        // CONNECT for the second with a 302, which refuses the tunnel.
        let (proxy_port, proxied, proxy_task) = reply_server(redirect).await;
        let proxy = format!("http://127.0.0.1:{proxy_port}");
        let url = format!("http://127.0.0.1:{origin_port}/start");
        let connect = format!("CONNECT localhost:{target_port} ");
        let taken = |log: &Log| std::mem::take(&mut *log.lock().unwrap());

        set_proxy_environment(&[("HTTPS_PROXY", &proxy)]);
        let results = reads_without_a_lane(&url).await;
        let (direct, dialed, through) = (taken(&origin), taken(&target), taken(&proxied));
        let hop = format!("redirect to https://localhost:{target_port} refused");
        assert!(
            results.iter().all(|result| matches!(
                result,
                Err(FetchError::ProxyConfig(message)) if message.contains(&hop)
            )) && direct.len() >= 2
                && dialed.is_empty()
                && through.is_empty(),
            "HTTPS_PROXY alone: {results:?}; the origin saw {} request(s), the target took {} \
             connection(s), the proxy saw {through:?}",
            direct.len(),
            dialed.len()
        );

        set_proxy_environment(&[("HTTP_PROXY", &proxy), ("HTTPS_PROXY", &proxy)]);
        let results = reads_without_a_lane(&url).await;
        let (direct, dialed, through) = (taken(&origin), taken(&target), taken(&proxied));
        let count = |prefix: &str| {
            through
                .iter()
                .filter(|head| head.starts_with(prefix))
                .count()
        };
        assert!(
            results
                .iter()
                .all(|result| result.is_err() && !matches!(result, Err(FetchError::ProxyConfig(_))))
                && count(&format!("GET {url} ")) >= 2
                && count(&connect) >= 2
                && direct.is_empty() && dialed.is_empty(),
            "both proxies: {results:?}; the origin saw {} request(s), the target took {} \
             connection(s), the proxy saw {through:?}",
            direct.len(),
            dialed.len()
        );

        set_proxy_environment(&[]);
        let results = reads_without_a_lane(&url).await;
        let (direct, dialed, through) = (taken(&origin), taken(&target), taken(&proxied));
        origin_task.abort();
        target_task.abort();
        proxy_task.abort();
        assert!(
            results
                .iter()
                .all(|result| result.is_err() && !matches!(result, Err(FetchError::ProxyConfig(_))))
                && direct.len() >= 2
                && dialed.len() >= 2 && through.is_empty(),
            "no proxy: {results:?}; the origin saw {} request(s), the target took {} \
             connection(s), the proxy saw {through:?}",
            direct.len(),
            dialed.len()
        );
    }
}
