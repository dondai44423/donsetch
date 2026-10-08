//! Fetch orchestrator with temporal stealth: origin pool, TLS session
//! resumption, persistent cookie jar, conditional revalidation cache,
//! Happy Eyeballs, single idempotent retry.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::detect::walls::{self, Verdict};
use crate::error::FetchError;
use crate::ghost::cache::CookieRecord;
use crate::profile::{BrowserProfile, RequestClass};
use crate::transport::pool::Pool;
use crate::transport::request_route::RequestRoute;
use crate::transport::{h1, h2::conn::H2Conn, proxy, tcp, tls};

use super::cookies::CookieJar;
use super::decompress;
use super::revalidate::{CacheCheck, RevalidationCache};

const MAX_REDIRECTS: u8 = 10;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheState {
    None,
    /// Served from a fresh cache window, no request was made.
    Fresh,
    /// Server said 304; body merged from cache.
    Revalidated,
}

pub struct FetchOutcome {
    /// Routing policy selected before the first request, retained for recovery.
    pub route: RequestRoute,
    /// Final URL after redirects.
    pub url: String,
    pub status: u16,
    pub alpn: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub redirects: u8,
    pub cache: CacheState,
    /// True when the request rode a pooled (pre-existing) connection.
    pub used_pool: bool,
    pub verdict: Verdict,
    pub elapsed: Duration,
}

/// Per-request identity knobs. Copy: the redirect driver hands
/// the same identity to every hop.
#[derive(Clone, Copy)]
struct RequestIdentity<'a> {
    class: RequestClass,
    legacy_user_agent: Option<&'a str>,
    /// Persona-coherent Accept-Language (v4 E2). None = TLD/script default.
    accept_language: Option<&'a str>,
}

pub struct Fetcher {
    profile: BrowserProfile,
    connector: boring::ssl::SslConnector,
    /// The interception-safe connector used for HTTP CONNECT proxy
    /// hops: TLS-intercepting middleboxes re-terminate with a second
    /// stack and some reset on GREASE/ALPS/compress_cert ClientHellos.
    /// SOCKS5 tunnels do TLS end-to-end and keep Chrome-true.
    connector_compat: boring::ssl::SslConnector,
    sessions: tls::SessionStore,
    pool: Mutex<Pool>,
    jar: Mutex<CookieJar>,
    cache: Mutex<RevalidationCache>,
    /// Shared egress fabric (v4 A2). When present and a proxy pool
    /// is configured, fetch sticks to one lane per host and rotates
    /// on 429/407/dead/timeout. None = historical env/slot path.
    egress: Option<std::sync::Arc<crate::search::egress::EgressPool>>,
}

impl Fetcher {
    /// Warm = a cached TLS session under `origin`: the repeat-navigation
    /// signal that flips TFO on at the TCP layer (Linux).
    fn sessions_has(&self, origin: &str) -> bool {
        tls::has_session(&self.sessions, &self.connector, origin)
    }

    pub fn new(profile: BrowserProfile) -> Result<Self, FetchError> {
        let sessions = tls::new_session_store();
        let connector = tls::build_connector(&profile)?;
        let connector_compat = tls::build_connector_compat(&profile)?;
        Ok(Self {
            profile,
            connector,
            connector_compat,
            sessions,
            pool: Mutex::new(Pool::new()),
            jar: Mutex::new(CookieJar::new()),
            cache: Mutex::new(RevalidationCache::new()),
            egress: None,
        })
    }

    /// Attach the process-wide egress pool so fetch can stick to a
    /// host lane and rotate on rate-limit signals (v4 A2).
    pub fn with_egress(mut self, pool: std::sync::Arc<crate::search::egress::EgressPool>) -> Self {
        self.egress = Some(pool);
        self
    }

    pub fn profile(&self) -> &BrowserProfile {
        &self.profile
    }

    /// Import cookies harvested by DonGhost (tier-2
    /// solve) into the persistent jar so the tier-1
    /// re-fetch carries the clearance.
    pub async fn import_cookies(&self, cookies: &[CookieRecord]) {
        let mut jar = self
            .jar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for c in cookies {
            jar.store_raw(c);
        }
    }

    /// Import VAULT-backed cookies (session harvests, the boot vault
    /// replay): as `import_cookies`, and the domains register as
    /// vault-fed so a login/logout jar rebuild really clears them —
    /// an unregistered domain is invisible to the reset and its dead
    /// session survives the logout in the jar.
    pub async fn import_vault_cookies(&self, cookies: &[CookieRecord]) {
        let mut jar = self
            .jar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jar.import_vault(cookies);
    }

    /// Replace the jar wholesale from the session vault (login or
    /// logout just happened on disk). Anything not in `cookies` is
    /// gone, which is exactly what a logout requires.
    pub async fn reset_to(&self, cookies: &[CookieRecord]) {
        let mut jar = self
            .jar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jar.reset(cookies);
    }

    /// Whole-jar export for the tier-1 cookie vault (v4 phase 1.4):
    /// the browser cookie store view that makes a returning agent
    /// replay like a returning device across process restarts.
    pub async fn jar_all_snapshot(&self) -> Vec<CookieRecord> {
        let jar = self.jar.lock().unwrap_or_else(|e| e.into_inner());
        jar.snapshot_all()
    }

    /// Export all cookies for a host with their expiry, for
    /// write-back to the persistent domain profile after a
    /// successful warm fetch.
    pub fn jar_snapshot(&self, host: &str) -> Vec<CookieRecord> {
        let jar = self
            .jar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jar.snapshot_for(host)
    }

    /// Fetch with browser-correct redirects, cookies, cache revalidation.
    pub async fn fetch(&self, url_str: &str) -> Result<FetchOutcome, FetchError> {
        self.fetch_via(url_str, None).await
    }

    /// Fetch through a specific egress lane (proxy). Redirects,
    /// cookies, revalidation all follow the lane : pool keys are
    /// proxy-scoped so egress IPs never share conns.
    pub async fn fetch_via(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar(url_str, proxy, true).await
    }

    /// Full lane control: `use_jar=false` keeps the shared cookie
    /// jar OUT of the request. Proxy lanes stay unlinked : the
    /// direct lane's session cookies must never transit a third
    /// egress IP.
    pub async fn fetch_via_jar(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_ref(url_str, proxy, use_jar, None).await
    }

    /// Evidence-grade cold probe (v4 phase 0.2): no shared cookie
    /// jar (a true cold client) and the revalidation cache bypassed
    /// (a cached page is not evidence about the wall RIGHT NOW).
    /// Used only by the background route-memory prober.
    pub async fn fetch_cold_probe(&self, url_str: &str) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_opts(url_str, None, false, None, true, true)
            .await
    }

    /// Same as `fetch_via_jar` but with a referer header. The
    /// referer is sent on the initial request only (not redirect
    /// hops), matching browser behavior. `sec-fetch-site` is
    /// computed from the referer's origin vs the target's origin:
    /// `same-origin` or `cross-site`. No referer → `none` (typed
    /// URL, the default).
    pub async fn fetch_via_jar_ref(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_opts(url_str, proxy, use_jar, referer, false, true)
            .await
    }

    /// Full-knobs variant: `skip_cache` bypasses the revalidation
    /// cache entirely (probe path only; everything else keeps it).
    /// `pool_pick` gates the opt-in pool lane (`proxy.fetch_rotate`);
    /// crawl passes `false` because it owns its lane choice.
    pub async fn fetch_via_jar_opts(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        skip_cache: bool,
        pool_pick: bool,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_identity(
            url_str,
            proxy,
            use_jar,
            referer,
            skip_cache,
            pool_pick,
            RequestIdentity {
                class: RequestClass::Navigation,
                legacy_user_agent: None,
                accept_language: None,
            },
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_via_jar_identity(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        skip_cache: bool,
        pool_pick: bool,
        identity: RequestIdentity<'_>,
        route_override: Option<&RequestRoute>,
    ) -> Result<FetchOutcome, FetchError> {
        // Centralized URL safety gate (fetch tier). The synchronous
        // literal checks run here (scheme, credentials, localhost and
        // private literals: no dial can follow a cached return). The
        // async DNS-aware tier runs exactly once per request, inside
        // fetch_once_via; this outer gate used to resolve DNS too,
        // doubling resolver RTT and load on every fetch (L7).
        crate::fetch::guards::validate_url_basic(url_str)?;
        let started = Instant::now();

        // Pool-aware fetch lane, opt-in (`proxy.fetch_rotate`, off
        // by default; `donsetch proxy fetch on` turns it on). When
        // enabled, no explicit proxy is passed, and a pool exists,
        // stick to one exit per host for the whole redirect chain
        // (never mid-200-session) and rotate on 429 / 407 /
        // connect-dead / timeout. Crawl passes `pool_pick=false`
        // (it owns its lane choice), and with the opt-in off this is
        // the historical env/slot path: one request does not
        // rate-limit, and proxies cost bandwidth + TLS fidelity.
        let fetch_host = url::Url::parse(url_str)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        let pool_lane = if route_override.is_none()
            && pool_pick_enabled(
                proxy,
                pool_pick,
                crate::config::cfg().proxy.fetch_rotate,
                self.egress.as_deref(),
            ) {
            self.egress
                .as_ref()
                .and_then(|pool| pool.pick_fetch(&fetch_host, true))
        } else {
            None
        };
        // A pool answer without a proxy leaves configured routing in charge.
        // The selected pool host/id travel with the route so related hops and
        // browser replay teach the original assignment without picking again.
        let route = route_override.cloned().unwrap_or_else(|| {
            if let Some(proxy) = proxy {
                RequestRoute::pinned(proxy.clone())
            } else if pool_lane_is_proxy(pool_lane.as_ref()) {
                let lane = pool_lane.as_ref().expect("proxy lane");
                RequestRoute::pooled(
                    lane.proxy.clone().expect("proxy lane"),
                    fetch_host.clone(),
                    lane.id.clone(),
                )
            } else {
                RequestRoute::configured()
            }
        });
        let pool_lane = route.pool_lane();
        let initial_proxy = route.proxy_for(url_str)?;
        let initial_url =
            url::Url::parse(url_str).map_err(|_| FetchError::InvalidUrl(url_str.into()))?;
        let initial_headers =
            self.request_headers(&initial_url, &[], use_jar, referer, identity)?;
        let cache_key =
            Self::representation_key(url_str, initial_proxy.as_ref(), use_jar, &initial_headers);
        let check = if skip_cache {
            CacheCheck::None
        } else {
            self.cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .check(&cache_key)
        };
        let (conditional, validation) = match check {
            CacheCheck::Fresh(body, status, headers) => {
                let verdict = walls::detect(status, &headers, &body);
                return Ok(FetchOutcome {
                    route,
                    url: url_str.into(),
                    status,
                    alpn: "cache".into(),
                    headers,
                    body,
                    redirects: 0,
                    cache: CacheState::Fresh,
                    used_pool: false,
                    verdict,
                    elapsed: started.elapsed(),
                });
            }
            CacheCheck::Revalidate(conditional, snapshot) => (conditional, Some(snapshot)),
            CacheCheck::None => (Vec::new(), None),
        };
        let mut current = url_str.to_string();
        let mut redirects = 0u8;
        let mut first_request = true;

        // Evaluate captured protocol/bypass settings for each current URL.
        // Explicit and pool routes remain pinned across the redirect chain.

        loop {
            let hop_proxy = if first_request {
                initial_proxy.clone()
            } else {
                route.proxy_for(&current)?
            };
            let effective_proxy = hop_proxy.as_ref();
            // Referer applies to the initial request only.
            // Redirects get no referer (avoids cross-origin leak).
            let ref_arg = if first_request { referer } else { None };
            let mut wire_headers = if first_request {
                initial_headers.clone()
            } else {
                let url = url::Url::parse(&current)
                    .map_err(|_| FetchError::InvalidUrl(current.clone()))?;
                self.request_headers(&url, &[], use_jar, ref_arg, identity)?
            };
            let hop_key =
                Self::representation_key(&current, effective_proxy, use_jar, &wire_headers);
            // Conditionals validate this context rather than define another
            // representation. Vary on these fields is conservatively uncacheable.
            if first_request {
                wire_headers.extend(conditional.iter().cloned());
            }
            let hop_started = Instant::now();
            let mut out = match self
                .fetch_once_with_headers(&current, effective_proxy, use_jar, wire_headers)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    if let (Some(pool), Some((host, id))) = (&self.egress, pool_lane) {
                        note_lane_outcome(pool, host, id, &e);
                    }
                    return Err(e);
                }
            };
            out.route = route.clone();
            if let (Some(pool), Some((host, id))) = (&self.egress, pool_lane) {
                match out.status {
                    429 => pool.note_fetch_rate_limited(host, id),
                    200..=299 | 304 => {
                        pool.report_ok(host, id);
                        pool.observe_rtt(id, hop_started.elapsed());
                    }
                    _ => {}
                }
            }
            // Cookie store for this hop lives in fetch_once_via_class
            // (v4 phase 2.1): the primitive owns the jar-write, so the
            // cookie-warm retry below can already ride cookies this
            // hop just set.

            // Only this hop's validators authorize a 304 merge. Redirects
            // and unsolicited 304s cannot inherit the original body's identity.
            if out.status == 304 {
                let Some(snapshot) = validation.as_ref().filter(|_| first_request) else {
                    return Err(FetchError::Http(
                        "304 response without a matching request validator snapshot".into(),
                    ));
                };
                let (body, status, headers) = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .revalidated(&cache_key, snapshot, &out.headers, hop_started.elapsed())
                    .map_err(FetchError::Http)?;
                out.status = status;
                out.headers = headers;
                out.body = body;
                out.cache = CacheState::Revalidated;
                // fetch_once_via already scored the bare 304, where
                // detect() sees an empty body and has no 3xx arm : it
                // returns Blocked. Re-score the merged body, as the
                // CacheCheck::Fresh arm does for its cached body;
                // otherwise every revalidated page comes back as
                // "Blocked status=200".
                out.verdict = walls::detect(out.status, &out.headers, &out.body);
                out.elapsed = started.elapsed();
                out.redirects = redirects;
                return Ok(out);
            }

            // A full response supersedes the old representation, even when
            // it is a redirect, a wall or a deletion that we cannot cache.
            // An older in-flight 304 may still finish from its own snapshot,
            // but cannot resurrect that retired entry.
            self.cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&hop_key);

            match out.status {
                301 | 302 | 303 | 307 | 308 => {
                    redirects += 1;
                    first_request = false;
                    if redirects > MAX_REDIRECTS {
                        return Err(FetchError::TooManyRedirects);
                    }
                    let Some(loc) = header_value(&out.headers, "location") else {
                        out.elapsed = started.elapsed();
                        out.redirects = redirects;
                        return Ok(out);
                    };
                    let base = url::Url::parse(&current)
                        .map_err(|_| FetchError::InvalidUrl(current.clone()))?;
                    // Centralized redirect SSRF guard : validates scheme,
                    // credentials and host, and rejects private literals.
                    // Non-http(s) redirects are returned honestly, not followed.
                    // The async DNS-aware gate for the new target runs once,
                    // inside fetch_once_via (it re-gates every URL it is
                    // handed); a second call here would resolve DNS twice
                    // per hop for the same verdict.
                    let next = match crate::fetch::guards::validate_redirect_url(&base, &loc) {
                        Ok(u) => u,
                        Err(e) => {
                            // Non-web scheme: return honestly per original
                            // behavior (file://, ftp:// etc. not followed).
                            if e.to_string().contains("non-http") {
                                out.elapsed = started.elapsed();
                                out.redirects = redirects;
                                return Ok(out);
                            }
                            return Err(e);
                        }
                    };
                    current = next.to_string();
                }
                _ => {
                    out.verdict = walls::detect(out.status, &out.headers, &out.body);

                    // Only real content enters the revalidation cache.
                    // A challenge interstitial with an ETag would
                    // otherwise be re-served fresh as "content" on
                    // every later fetch (hardcoded ContentOk made it
                    // worse). Walls are never cacheable.
                    if !skip_cache && matches!(out.verdict, Verdict::ContentOk) {
                        let mut cache = self
                            .cache
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        cache.store_with_delay(
                            &hop_key,
                            out.status,
                            &out.headers,
                            &out.body,
                            hop_started.elapsed(),
                        );
                    }

                    // One cookie-warm retry uses the newly stored cookie, and
                    // its body is keyed by the headers it actually sent.
                    if matches!(out.verdict, Verdict::Challenge(_))
                        && header_value(&out.headers, "set-cookie").is_some()
                    {
                        let url = url::Url::parse(&current)
                            .map_err(|_| FetchError::InvalidUrl(current.clone()))?;
                        let headers =
                            self.request_headers(&url, &[], use_jar, ref_arg, identity)?;
                        let retry_key =
                            Self::representation_key(&current, effective_proxy, use_jar, &headers);
                        let retry_started = Instant::now();
                        if let Ok(mut retry) = self
                            .fetch_once_with_headers(&current, effective_proxy, use_jar, headers)
                            .await
                        {
                            if retry.status == 304 {
                                return Err(FetchError::Http(
                                    "304 response to cookie-warm retry without request validators"
                                        .into(),
                                ));
                            }
                            retry.verdict =
                                walls::detect(retry.status, &retry.headers, &retry.body);
                            self.cache
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&retry_key);
                            if !skip_cache && matches!(retry.verdict, Verdict::ContentOk) {
                                self.cache
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .store_with_delay(
                                        &retry_key,
                                        retry.status,
                                        &retry.headers,
                                        &retry.body,
                                        retry_started.elapsed(),
                                    );
                            }
                            out = retry;
                            out.route = route.clone();
                        }
                    }

                    out.elapsed = started.elapsed();
                    out.redirects = redirects;
                    return Ok(out);
                }
            }
        }
    }

    /// Same, optionally through a CONNECT proxy. Pool keys
    /// are proxy-scoped so egress IPs never share conns.
    /// `use_jar=false` keeps cookies out entirely : search
    /// engines get cookie-less requests so egress lanes
    /// stay unlinked and the fetch-tool jar stays clean.
    pub async fn fetch_once_via(
        &self,
        url_str: &str,
        conditional: &[(String, String)],
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_once_via_class(
            url_str,
            conditional,
            proxy,
            use_jar,
            referer,
            RequestClass::Navigation,
        )
        .await
    }

    /// Class-aware variant (v4 phase 1.2): subresource fetches
    /// carry the per-class header set, not the navigation set.
    pub async fn fetch_once_via_class(
        &self,
        url_str: &str,
        conditional: &[(String, String)],
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        class: RequestClass,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_once_via_identity(
            url_str,
            conditional,
            proxy,
            use_jar,
            referer,
            RequestIdentity {
                class,
                legacy_user_agent: None,
                accept_language: None,
            },
        )
        .await
    }

    /// One cookie-less hop with a request-local legacy User-Agent.
    /// Reuses TLS, connection pooling and URL guards; does not mutate the
    /// shared browser profile or follow redirects with this identity.
    pub async fn fetch_once_via_user_agent(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        user_agent: &str,
    ) -> Result<FetchOutcome, FetchError> {
        if user_agent.is_empty()
            || !user_agent.is_ascii()
            || user_agent.bytes().any(|b| b.is_ascii_control())
        {
            return Err(FetchError::Http("invalid User-Agent".into()));
        }
        self.fetch_once_via_identity(
            url_str,
            &[],
            proxy,
            false,
            None,
            RequestIdentity {
                class: RequestClass::Navigation,
                legacy_user_agent: Some(user_agent),
                accept_language: None,
            },
        )
        .await
    }

    /// Tier-1 navigation-identity fetch (the daemon's Chrome-class
    /// hop). Follows redirects (bounded); cross-host hops are
    /// re-gated per hop. Honors the fetch pool opt-in like every
    /// other fetch path (`proxy.fetch_rotate`).
    pub async fn fetch_persona(
        &self,
        url_str: &str,
        accept_language: Option<&str>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_persona_on_route(url_str, accept_language, None)
            .await
    }

    /// Select once for the whole fetch call, including related fallback URLs.
    pub fn route_for_fetch(&self, url: &str) -> RequestRoute {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        if crate::config::cfg().proxy.fetch_rotate
            && let Some(lane) = self
                .egress
                .as_ref()
                .and_then(|pool| pool.pick_fetch(&host, true))
            && let Some(proxy) = lane.proxy
        {
            return RequestRoute::pooled(proxy, host, lane.id);
        }
        RequestRoute::configured()
    }

    /// Repeat HTTP under the policy that produced the browser document.
    pub async fn fetch_persona_on_route(
        &self,
        url_str: &str,
        accept_language: Option<&str>,
        route: Option<&RequestRoute>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_identity(
            url_str,
            None,
            true,
            None,
            false,
            true,
            RequestIdentity {
                class: RequestClass::Navigation,
                legacy_user_agent: None,
                accept_language,
            },
            route,
        )
        .await
    }

    fn request_headers(
        &self,
        url: &url::Url,
        conditional: &[(String, String)],
        use_jar: bool,
        referer: Option<&str>,
        identity: RequestIdentity<'_>,
    ) -> Result<Vec<(String, String)>, FetchError> {
        let RequestIdentity {
            class,
            legacy_user_agent: user_agent,
            accept_language,
        } = identity;
        let host = url
            .host_str()
            .ok_or_else(|| FetchError::InvalidUrl(url.to_string()))?;
        let is_https = url.scheme() == "https";
        let default_port = if is_https { 443 } else { 80 };
        let authority = if url.port_or_known_default() == Some(default_port) {
            host.to_owned()
        } else {
            format!(
                "{host}:{}",
                url.port()
                    .ok_or_else(|| FetchError::InvalidUrl(url.to_string()))?
            )
        };
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        // Header set from profile (Chrome order, coherence) + cookie + conditionals.
        let mut req_headers = self.profile.h1_headers_for_class(&authority, &path, class);
        if let Some(al) = accept_language
            && !al.is_empty()
        {
            for (n, v) in &mut req_headers {
                if n == "accept-language" {
                    *v = al.to_owned();
                }
            }
        }
        if let Some(ua) = user_agent {
            // These browser metadata headers do not describe a legacy client.
            req_headers.retain(|(n, _)| {
                !n.starts_with("sec-ch-ua")
                    && !n.starts_with("sec-fetch-")
                    && n != "upgrade-insecure-requests"
            });
            for (name, value) in &mut req_headers {
                if name == "user-agent" {
                    *value = ua.to_owned();
                }
            }
        }
        if use_jar {
            let jar = self
                .jar
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cookie) = jar.header_for(host, url.path(), is_https) {
                // Chrome 151 capture: cookie sits after sec-fetch-dest,
                // before accept-encoding.
                let pos = req_headers
                    .iter()
                    .position(|(n, _)| n == "accept-encoding")
                    .unwrap_or(req_headers.len());
                req_headers.insert(pos, ("cookie".into(), cookie));
            }
        }
        // Basic auth from URL userinfo (user:pass@host). The url
        // crate strips userinfo from the authority we send in the
        // Host header (correct per RFC 3986), so we carry the
        // credentials as an Authorization: Basic header, matching
        // browser behavior. Without this, every tier-1 request to
        // a basic-auth URL goes out unauthenticated (issue #15).
        if !url.username().is_empty() {
            let credentials = match url.password() {
                Some(pass) => format!("{}:{}", url.username(), pass),
                None => url.username().to_string(),
            };
            let encoded = crate::transport::proxy::base64(&credentials);
            let pos = req_headers
                .iter()
                .position(|(n, _)| n == "accept-encoding")
                .unwrap_or(req_headers.len());
            req_headers.insert(pos, ("authorization".into(), format!("Basic {encoded}")));
        }
        req_headers.extend(conditional.iter().cloned());

        // Referer + sec-fetch-site: when following a link, a real
        // browser sends `Referer` and sets `sec-fetch-site` to
        // `same-origin` or `cross-site` (never `none` : that's
        // for typed URLs only). Without this, every crawl request
        // looks like a fresh typed navigation, which is a bot
        // fingerprint.
        if let Some(ref_url) = referer {
            let site = sec_fetch_site(ref_url, url.as_str());
            if let Some(pos) = req_headers.iter().position(|(n, _)| n == "sec-fetch-site") {
                req_headers[pos].1 = site.into();
            }
            // Chrome puts Referer after Sec-Fetch-Dest, before
            // Accept-Encoding.
            let ref_val = referer_value(ref_url, url.as_str());
            let pos = req_headers
                .iter()
                .position(|(n, _)| n == "accept-encoding")
                .unwrap_or(req_headers.len());
            req_headers.insert(pos, ("referer".into(), ref_val));
        }

        Ok(req_headers)
    }

    /// Hash the selected route and exact headers. No cookie, credential or
    /// URL parameter is retained in the cache key or exposed in diagnostics.
    fn representation_key(
        url: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        headers: &[(String, String)],
    ) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update([u8::from(use_jar)]);
        let route = proxy.map_or_else(|| "direct".to_owned(), proxy::Proxy::connection_key);
        for field in std::iter::once(url)
            .chain(std::iter::once(route.as_str()))
            .chain(
                headers
                    .iter()
                    .flat_map(|(name, value)| [name.as_str(), value.as_str()]),
            )
        {
            hash.update((field.len() as u64).to_be_bytes());
            hash.update(field.as_bytes());
        }
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    async fn fetch_once_via_identity(
        &self,
        url_str: &str,
        conditional: &[(String, String)],
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        identity: RequestIdentity<'_>,
    ) -> Result<FetchOutcome, FetchError> {
        let url = url::Url::parse(url_str).map_err(|_| FetchError::InvalidUrl(url_str.into()))?;
        let headers = self.request_headers(&url, conditional, use_jar, referer, identity)?;
        let mut out = self
            .fetch_once_with_headers(url_str, proxy, use_jar, headers)
            .await?;
        out.route = proxy
            .cloned()
            .map(RequestRoute::pinned)
            .unwrap_or_else(RequestRoute::direct);
        Ok(out)
    }

    async fn fetch_once_with_headers(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        req_headers: Vec<(String, String)>,
    ) -> Result<FetchOutcome, FetchError> {
        // Centralized gate ensures credentials/host checks even for
        // direct fetch_once calls (e.g. tests, internal callers).
        // Includes DNS resolution : every target, including proxy
        // lanes, is checked before any TCP connect.
        crate::fetch::guards::ensure_url_safe(url_str).await?;
        let url = url::Url::parse(url_str).map_err(|_| FetchError::InvalidUrl(url_str.into()))?;
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(FetchError::InvalidUrl(url_str.into()));
        }
        let is_https = scheme == "https";
        let host = url
            .host_str()
            .ok_or_else(|| FetchError::InvalidUrl(url_str.into()))?;
        let default_port = if is_https { 443 } else { 80 };
        let port = url.port().unwrap_or(default_port);
        let mut path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        };
        if path.is_empty() {
            path = "/".into();
        }
        let authority = if port == default_port {
            host.to_string()
        } else {
            format!("{host}:{port}")
        };
        let origin = match proxy {
            Some(p) => format!("{}|{}", p.connection_key(), authority),
            None => authority.clone(),
        };

        // Reject header values carrying CR/LF/NUL before they can
        // reach the wire: values synthesized from response data
        // (cookies, referer) must never split the request.
        if req_headers.iter().any(|(n, v)| {
            !crate::fetch::guards::valid_header_value(n)
                || !crate::fetch::guards::valid_header_value(v)
        }) {
            return Err(FetchError::Http(
                "invalid header value (CR/LF/NUL) : refused to send".into(),
            ));
        }

        // 0) h3 lane (v4 phase 5.1). Direct egress only (UDP does not
        // tunnel through CONNECT): the h1/h2 path stays the fallback
        // there. Route memory + kill switch gate it. The attempt's
        // transport failure drops the route (Chrome semantics: a served
        // alt-svc that fails vanishes until a header re-vouches).
        if is_https
            && proxy.is_none()
            && crate::config::cfg().fetch.h3
            && let Some(h3port) = crate::transport::routes::h3_route(&origin, "direct")
        {
            match crate::transport::h3::h3_fetch_direct(
                host,
                h3port,
                &path,
                &authority,
                req_headers.clone(),
                None,
                // The caller's identity, not a fresh chrome_150 built per
                // request: the QUIC config and the TLS tables it carries
                // must be the ones this fetch is presenting everywhere
                // else.
                self.profile(),
            )
            .await
            {
                Ok((h3out, _stats)) => {
                    if let Some(alt) = h3out.altsvc.as_ref() {
                        crate::transport::routes::absorb_alt_svc(&origin, alt, "direct");
                    }
                    if h3out.status == 0 || h3out.status < 200 {
                        crate::transport::routes::drop_h3(&origin);
                        // fall through to h1/h2
                    } else {
                        self.store_hop_cookies(use_jar, host, is_https, &h3out.headers);
                        // Same exit as every other transport: finish()
                        // decompresses and scores walls::detect, so a
                        // challenge served over h3 escalates instead of
                        // masquerading as ContentOk. An undecodable h3
                        // payload is a transport failure: drop the
                        // vouch, let h1/h2 answer.
                        match finish(
                            url_str.to_string(),
                            "h3",
                            h3out.status,
                            h3out.headers,
                            h3out.body,
                            false,
                        ) {
                            Ok(out) => return Ok(out),
                            Err(_) => crate::transport::routes::drop_h3(&origin),
                        }
                    }
                }
                Err(_) => {
                    crate::transport::routes::drop_h3(&origin);
                    // fall through to h1/h2 below
                }
            }
        }

        // 1) H2 here is TLS-only. A plaintext URL with the same authority
        // must never borrow an HTTPS socket or send a :scheme=https request.
        let pooled = if is_https {
            self.pool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take_h2(&origin)
        } else {
            None
        };
        if let Some(mut conn) = pooled {
            match self
                .h2_request(&mut conn, &authority, &path, &req_headers, true)
                .await
            {
                Ok(out) => {
                    // verdict already scored by finish()
                    self.store_hop_cookies(use_jar, host, is_https, &out.headers);
                    // Alt-svc absorb (v4 phase 5.1): only on a direct
                    // https lane; proxies naturally exempt. It lets a
                    // later connection on the same origin take h3, for
                    // exactly the ma= lifetime the server vouched.
                    if is_https
                        && proxy.is_none()
                        && let Some((_, hdr_alt)) = out
                            .headers
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case("alt-svc"))
                    {
                        crate::transport::routes::absorb_alt_svc(&origin, hdr_alt, "direct");
                    }
                    self.pool
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .put_h2(&origin, conn);
                    return Ok(out);
                }
                Err(_) => { /* conn died; drop it and go fresh */ }
            }
        }

        // 2) Fresh connection, one retry on network failure (Chrome-true).
        let mut last_err = FetchError::Http("unreachable".into());
        for attempt in 0..2 {
            match self
                .fresh_request(
                    is_https,
                    &origin,
                    host,
                    port,
                    &authority,
                    &path,
                    &req_headers,
                    proxy,
                )
                .await
            {
                Ok(out) => {
                    // verdict already scored by finish()
                    self.store_hop_cookies(use_jar, host, is_https, &out.headers);
                    // Alt-svc absorb (v4 phase 5.1): only on a direct https
                    // lane; refreshed per response so the ma= lifetime stays
                    // current (the server's own ma=, never a constant). h3
                    // only when the server announced it for the same origin.
                    if is_https
                        && proxy.is_none()
                        && let Some((_, hdr_alt)) = out
                            .headers
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case("alt-svc"))
                    {
                        crate::transport::routes::absorb_alt_svc(&origin, hdr_alt, "direct");
                    }
                    return Ok(out);
                }
                Err(e) => {
                    // Retrying unchanged credentials cannot repair CONNECT 407
                    // and can replace the auth evidence with a later dial error.
                    if lane_note(&e) == Some(LaneNote::AuthFail) {
                        return Err(e);
                    }
                    last_err = e;
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }
        Err(last_err)
    }

    /// One jar-write owner (v4 phase 2.1): every use_jar hop both
    /// ATTACHES stored cookies (above, before dialing) and STORES the
    /// response's Set-Cookie (here, on success). Before this, only the
    /// redirect-loop wrapper in fetch_via_jar_opts stored, so one-hop
    /// jar riders (search prewarm, shadow subresources) read like a
    /// browser but learned nothing back: the jar stayed empty and the
    /// next request went out cookie-less. Real browsers store
    /// subresource Set-Cookie too, so the store lives in the shared
    /// primitive, not the callers. The primitive is strictly one-hop,
    /// so keying on the request host/scheme is per-hop correct for
    /// redirect chains.
    fn store_hop_cookies(
        &self,
        use_jar: bool,
        host: &str,
        is_https: bool,
        headers: &[(String, String)],
    ) {
        if !use_jar {
            return;
        }
        let mut jar = self
            .jar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jar.store_from_headers(host, headers, is_https);
    }

    #[allow(clippy::too_many_arguments)]
    async fn fresh_request(
        &self,
        is_https: bool,
        origin: &str,
        host: &str,
        port: u16,
        authority: &str,
        path: &str,
        req_headers: &[(String, String)],
        proxy: Option<&proxy::Proxy>,
    ) -> Result<FetchOutcome, FetchError> {
        // TLS-only origin includes its port and exact proxy identity. The
        // connector generation further partitions tickets by profile/trust.
        // TFO consults the same key as the ticket callback.
        let session_key = origin;
        // Dial: https through an HTTP proxy goes through a CONNECT
        // tunnel; plaintext http:// through an HTTP proxy goes RAW
        // with an absolute-form request line (RFC 9112 3.2.2) —
        // CONNECT is for https only. SOCKS5 tunnels both; direct
        // dials use Happy Eyeballs.
        let tcp = match proxy {
            Some(p) if !is_https && p.is_http_connect() => p.connect_tcp().await?,
            Some(p) => p.connect(host, port).await?,
            None => {
                tcp::happy_connect_with(host, port, is_https && self.sessions_has(session_key))
                    .await?
            }
        };

        // ── Plaintext http://: raw TCP straight into h1. ──
        // No h2 over plaintext (no browser does h2c); no TLS,
        // no session resumption, no ALPN.
        if !is_https {
            let mut stream = tcp;
            // Plaintext http:// through a raw HTTP-proxy hop uses
            // absolute-form request targets (RFC 9112 3.2.2): the
            // proxy needs the full origin in the request line to
            // route it. ONLY that hop : a SOCKS5 tunnel is
            // transparent, so the ORIGIN reads this line, and no
            // browser sends an origin absolute-form (fingerprint;
            // same condition as the dial above).
            let raw_http_proxy = proxy.filter(|p| p.is_http_connect());
            let target = if raw_http_proxy.is_some() {
                url_of("http", authority, path)
            } else {
                path.to_string()
            };
            // The raw hop has no CONNECT to carry credentials : a
            // credentialed proxy needs Proxy-Authorization on the
            // request itself (consumed by the proxy, never
            // forwarded to the origin).
            let with_auth: Vec<(String, String)>;
            let req_headers = match raw_http_proxy.and_then(|p| p.proxy_authorization()) {
                Some(auth) => {
                    let mut h = req_headers.to_vec();
                    h.push(("proxy-authorization".to_string(), auth));
                    with_auth = h;
                    &with_auth[..]
                }
                None => req_headers,
            };
            let resp =
                tokio::time::timeout(RESPONSE_TIMEOUT, h1::get(&mut stream, &target, req_headers))
                    .await
                    .map_err(|_| FetchError::Timeout)??;
            return finish(
                url_of("http", authority, path),
                "h1",
                resp.status,
                resp.headers,
                resp.body,
                false,
            );
        }

        // Http CONNECT hops get the interception-safe handshake:
        // they are almost always TLS-terminating middleboxes whose
        // second stack can reset on GREASE/ALPS/compress_cert, and
        // stealth is moot there (the proxy holds the plaintext).
        // SOCKS5 keeps the TLS end-to-end tunnel transparent, so it
        // keeps the Chrome-true wire profile.
        let through_http_proxy = proxy.is_some_and(|p| p.is_http_connect());
        let (connector, handshake) = if through_http_proxy {
            (
                &self.connector_compat,
                tls::HandshakeProfile::InterceptionSafe,
            )
        } else {
            (&self.connector, tls::HandshakeProfile::ChromeTrue)
        };
        let mut tls_stream = tokio::time::timeout(
            Duration::from_secs(15),
            tls::connect(
                &self.profile,
                connector,
                host,
                tcp,
                &self.sessions,
                session_key,
                handshake,
            ),
        )
        .await
        .map_err(|_| FetchError::Timeout)??;
        let alpn = tls_stream
            .ssl()
            .selected_alpn_protocol()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_else(|| "none".into());

        if alpn == "h2" {
            let mut conn = H2Conn::start(tls_stream, &self.profile).await?;
            let out = self
                .h2_request(&mut conn, authority, path, req_headers, false)
                .await?;
            self.pool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .put_h2(origin, conn);
            Ok(out)
        } else {
            let resp = tokio::time::timeout(
                RESPONSE_TIMEOUT,
                h1::get(&mut tls_stream, path, req_headers),
            )
            .await
            .map_err(|_| FetchError::Timeout)??;
            finish(
                url_of("https", authority, path),
                "h1",
                resp.status,
                resp.headers,
                resp.body,
                false,
            )
        }
    }

    async fn h2_request(
        &self,
        conn: &mut H2Conn,
        authority: &str,
        path: &str,
        req_headers: &[(String, String)],
        used_pool: bool,
    ) -> Result<FetchOutcome, FetchError> {
        let h2_headers: Vec<(String, String)> = req_headers
            .iter()
            .filter(|(n, _)| n != "host" && n != "connection")
            .cloned()
            .chain(std::iter::once(("priority".into(), "u=0, i".into())))
            .collect();
        let resp = tokio::time::timeout(RESPONSE_TIMEOUT, conn.get(authority, path, &h2_headers))
            .await
            .map_err(|_| FetchError::Timeout)??;
        finish(
            url_of("https", authority, path),
            "h2",
            resp.status,
            resp.headers,
            resp.body,
            used_pool,
        )
    }
}

fn url_of(scheme: &str, authority: &str, path: &str) -> String {
    format!("{scheme}://{authority}{path}")
}

fn finish(
    url: String,
    alpn: &str,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    used_pool: bool,
) -> Result<FetchOutcome, FetchError> {
    let encoding = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let body = if status == 204 || status == 304 {
        if !body.is_empty() {
            return Err(FetchError::Http(format!(
                "body bytes on bodyless HTTP {status} response"
            )));
        }
        body
    } else {
        decompress::decompress(&encoding, &body)?
    };
    // Wall classification lives here: every caller used to score the
    // finished outcome with walls::detect right after the call; one
    // site of truth instead of N re-detections (Q4).
    let verdict = walls::detect(status, &headers, &body);
    Ok(FetchOutcome {
        route: RequestRoute::direct(),
        url,
        status,
        alpn: alpn.into(),
        headers,
        body,
        redirects: 0,
        cache: CacheState::None,
        used_pool,
        verdict,
        elapsed: Duration::ZERO,
    })
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// Compute `sec-fetch-site` from the referer's origin vs the
/// target's origin. `same-origin` = same scheme+host+port;
/// everything else = `cross-site` (conservative : we don't
/// compute the registrable domain for `same-site`).
fn sec_fetch_site(referer: &str, target: &str) -> &'static str {
    let ref_origin = url::Url::parse(referer).ok().map(|u| {
        (
            u.scheme().to_string(),
            u.host_str().unwrap_or("").to_string(),
            u.port_or_known_default(),
        )
    });
    let tgt_origin = url::Url::parse(target).ok().map(|u| {
        (
            u.scheme().to_string(),
            u.host_str().unwrap_or("").to_string(),
            u.port_or_known_default(),
        )
    });
    match (ref_origin, tgt_origin) {
        (Some(r), Some(t)) if r == t => "same-origin",
        _ => "cross-site",
    }
}

/// Chrome's default referrer policy `strict-origin-when-cross-origin`:
/// same-origin = full URL, cross-origin = origin only.
fn referer_value(referer: &str, target: &str) -> String {
    let ref_url = url::Url::parse(referer).ok();
    let tgt_url = url::Url::parse(target).ok();
    let same_origin = match (&ref_url, &tgt_url) {
        (Some(r), Some(t)) => {
            r.scheme() == t.scheme()
                && r.host_str() == t.host_str()
                && r.port_or_known_default() == t.port_or_known_default()
        }
        _ => false,
    };
    if same_origin {
        referer.to_string()
    } else if let Some(r) = ref_url {
        let host = r.host_str().unwrap_or("");
        let port = r.port().map(|p| format!(":{p}")).unwrap_or_default();
        format!("{}://{host}{port}/", r.scheme())
    } else {
        referer.to_string()
    }
}

/// Which lane-health signal a transport failure carries, for the egress
/// pool. [`lane_note`] maps an error to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaneNote {
    Timeout,
    AuthFail,
    Dead,
    /// Origin-side TLS (certificate verify): the host rotates off
    /// the lane (pair probation), the lane itself stays healthy.
    OriginTls,
}

/// True when the pool handed back a real lane (a proxy), not the
/// `direct` egress.
fn pool_lane_is_proxy(lane: Option<&crate::search::egress::Egress>) -> bool {
    lane.is_some_and(|e| e.proxy.is_some())
}

/// Pool-pick gate for the fetch lanes. The fetch tool opts in via
/// `proxy.fetch_rotate` (default off, `donsetch proxy fetch on`);
/// crawl always passes `pool_pick=false` because it owns its lane
/// choice; a caller-supplied lane mutes the pick; an empty pool is
/// no pool. `explicit` is the lane the caller passed, if any.
fn pool_pick_enabled(
    explicit: Option<&proxy::Proxy>,
    pool_pick: bool,
    cfg_rotate: bool,
    pool: Option<&crate::search::egress::EgressPool>,
) -> bool {
    explicit.is_none() && pool_pick && cfg_rotate && pool.is_some_and(|p| p.has_proxies())
}

/// Lane health from a transport failure, by variant rather than by
/// prose.
///
/// #248 split the resolver failure out of `Io` and the resolver timeout
/// out of `Timeout` (`Dns`, `DnsTimeout`), and this site kept matching
/// only the old variants: a proxy lane whose own name stopped resolving
/// was never marked dead, and a resolver timeout was not counted at all
/// (the message reads "dns timeout", which `contains("timed out")`
/// never matched).
///
/// Lane accounting for one failed hop on a pool lane.
fn note_lane_outcome(
    pool: &crate::search::egress::EgressPool,
    host: &str,
    egress_id: &str,
    e: &FetchError,
) {
    match lane_note(e) {
        Some(LaneNote::Timeout) => pool.note_fetch_timeout(host, egress_id),
        Some(LaneNote::AuthFail) => pool.note_fetch_auth_fail(host, egress_id),
        Some(LaneNote::Dead) => pool.note_fetch_dead(host, egress_id),
        Some(LaneNote::OriginTls) => pool.note_fetch_origin_tls(host, egress_id),
        None => {}
    }
}

/// `Dns`/`DnsTimeout` are the ORIGIN's name, never the lane's: the
/// only producer is `transport::dns::resolve`, reached from the SSRF
/// guard (which resolves the target before any lane dials, proxy
/// lanes included) and from the direct dials. A lane's own name
/// failing arrives as `Io` from `TcpStream::connect` in
/// `transport::proxy`. Benching the lane for the target's NXDOMAIN
/// retired every lane after N dead hosts (or N pages redirecting to
/// one) and `pick_fetch` then fell through to direct: the fetch left
/// on the real address with rotation configured. A policy refusal
/// (`Ssrf`) is about the URL and a protocol error is about the
/// origin: neither says anything about the lane either.
fn lane_note(e: &FetchError) -> Option<LaneNote> {
    let msg = e.to_string();
    match e {
        FetchError::Dns(_) | FetchError::DnsTimeout(_) => None,
        FetchError::Timeout => Some(LaneNote::Timeout),
        // A certificate-verify failure is the certificate the far
        // side presented, not lane health: on a SOCKS5 tunnel and a
        // plain CONNECT tunnel alike the TLS peer is the origin, so a
        // self-signed/expired/mismatched origin cert used to retire a
        // healthy lane for every host for 10 minutes (the #248 class
        // again). The lane stays healthy; the HOST moves off it
        // (pair probation + rotation), because a lane that intercepts
        // TLS re-signs every host it carries and a sticky host would
        // otherwise retry the same lane forever (#296 review).
        // Egress-flavored TLS failures ("handshake aborted" / "cut
        // short": the classifier's tls.egress vocabulary) still
        // bench.
        FetchError::Tls(msg) if crate::transport::tls::is_cert_verify_failure(msg) => {
            Some(LaneNote::OriginTls)
        }
        FetchError::Io(_) | FetchError::Tls(_) => Some(LaneNote::Dead),
        // A proxy that demands credentials. Ahead of the text arms:
        // "CONNECT -> 407" must never read as a dead lane.
        _ if msg.contains("CONNECT -> 407") => Some(LaneNote::AuthFail),
        // The pre-#248 text arms, kept for the same Http shapes they
        // used to catch.
        _ if msg.contains("timed out") => Some(LaneNote::Timeout),
        _ if msg.contains("connection") || msg.contains("connect") => Some(LaneNote::Dead),
        _ => None,
    }
}

#[cfg(test)]
mod transport_exit_tests {
    use super::*;

    async fn owned_status_fetch(status: u16) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.proxy.fetch_rotate = true;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/owned-status", origin.local_addr().unwrap());
        let proxy =
            proxy::Proxy::parse(&format!("http://{}", proxy_listener.local_addr().unwrap()))
                .unwrap();
        let expected_head = format!("GET {url} HTTP/1.1\r\n");
        let server = tokio::spawn(async move {
            let (mut socket, _) = proxy_listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(socket.read_u8().await.unwrap());
            }
            assert!(head.starts_with(expected_head.as_bytes()));
            socket.write_all(format!("HTTP/1.1 {status} Owned\r\nContent-Length: 5\r\nConnection: close\r\n\r\nowned").as_bytes()).await.unwrap();
        });
        let pool = std::sync::Arc::new(crate::search::egress::EgressPool::new(vec![proxy.clone()]));
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default())
            .unwrap()
            .with_egress(std::sync::Arc::clone(&pool));
        let response = fetcher.fetch(&url).await.unwrap();
        server.await.unwrap();
        assert_eq!(response.status, status);
        assert_eq!(response.body, b"owned");
        assert_eq!(
            response.route.proxy_for(&url).unwrap().unwrap().id(),
            proxy.id()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), origin.accept())
                .await
                .is_err(),
            "a pooled success must not reach direct"
        );
        assert!(
            pool.rtt_ms(&proxy.id()).is_some(),
            "HTTP{status} must teach the selected pool lane RTT"
        );
    }

    #[tokio::test]
    async fn stealth_v3_success_status_201_updates_fetch_pool_health() {
        owned_status_fetch(201).await;
    }

    #[tokio::test]
    async fn stealth_v3_success_status_200_retains_fetch_pool_health() {
        owned_status_fetch(200).await;
    }

    // #248 split Dns/DnsTimeout out of Io/Timeout. Those variants
    // describe the ORIGIN's name (the guard resolves the target before
    // the lane dials; the lane's own name fails as Io from the proxy
    // connect), so they must leave lane health alone: the first sweep
    // benched the lane for the target's NXDOMAIN.
    #[test]
    fn lane_notes_know_the_dns_variants() {
        assert_eq!(
            lane_note(&FetchError::Dns(
                "could not resolve nope.invalid: no such host".into()
            )),
            None
        );
        assert_eq!(
            lane_note(&FetchError::DnsTimeout(
                "the resolver did not answer within 5s for nope.invalid".into()
            )),
            None
        );
        assert_eq!(lane_note(&FetchError::Timeout), Some(LaneNote::Timeout));
        assert_eq!(
            lane_note(&FetchError::Io(std::io::Error::other("connection refused"))),
            Some(LaneNote::Dead)
        );
        assert_eq!(
            lane_note(&FetchError::Http("CONNECT -> 407".into())),
            Some(LaneNote::AuthFail)
        );
        // Not the lane's fault: a policy refusal and an origin-side
        // protocol error leave lane health alone.
        assert_eq!(
            lane_note(&FetchError::Ssrf(
                "10.0.0.1 is a private/loopback address : SSRF guard".into()
            )),
            None
        );
        assert_eq!(lane_note(&FetchError::Http("parser died".into())), None);
    }

    // The vocabulary split: cert-verify failures are origin-side (the
    // host moves off the lane; the lane itself is never benched);
    // egress-flavored handshake failures stay lane-level.
    #[test]
    fn cert_verify_failures_leave_lane_health_alone() {
        assert_eq!(
            lane_note(&FetchError::Tls(
                "TLS certificate verification failed. The presentation cert chain was not issued by any trusted root.".into()
            )),
            Some(LaneNote::OriginTls)
        );
        assert_eq!(
            lane_note(&FetchError::Tls(
                "TLS handshake aborted (connection reset or cut mid-negotiation).".into()
            )),
            Some(LaneNote::Dead)
        );
        assert_eq!(
            lane_note(&FetchError::Tls(
                "TLS handshake cut short (peer closed the connection).".into()
            )),
            Some(LaneNote::Dead)
        );
    }

    // A bad origin certificate must not bench the lane, but the host
    // must still move off the lane that failed it: a sticky host on
    // a TLS-intercepting lane used to retry the same lane forever
    // (#296 review). The lane stays healthy for other hosts.
    #[test]
    fn a_bad_origin_certificate_moves_the_host_without_benching_the_lane() {
        use crate::search::egress::EgressPool;
        use crate::transport::proxy::Proxy;
        let dir = std::env::temp_dir().join(format!("donsetch-lane-cert-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let bad = Proxy::parse("http://127.0.0.1:34568").unwrap();
        let good = Proxy::parse("http://127.0.0.1:34569").unwrap();
        let pool = EgressPool::new(vec![bad.clone(), good.clone()]);
        let cert_fail = FetchError::Tls(
            "TLS certificate verification failed. The presentation cert chain was not issued by any trusted root."
                .into(),
        );
        // First pick: the first clean lane.
        assert_eq!(
            pool.pick_fetch("self-signed.example", true).map(|e| e.id),
            Some(bad.id()),
            "a healthy proxy lane is picked first"
        );
        note_lane_outcome(&pool, "self-signed.example", &bad.id(), &cert_fail);
        assert!(
            !pool.is_dead(&bad.id()),
            "an origin certificate is not the lane's fault"
        );
        assert_eq!(
            pool.pick_fetch("self-signed.example", true).map(|e| e.id),
            Some(good.id()),
            "the host rotates off the lane that failed it"
        );
        // A second failure on the same pair only hardens it (probation
        // to burned): the lane still never benches for an origin
        // certificate.
        note_lane_outcome(&pool, "self-signed.example", &bad.id(), &cert_fail);
        assert!(!pool.is_dead(&bad.id()));
        // An egress-flavored TLS failure is still a dead lane.
        note_lane_outcome(
            &pool,
            "self-signed.example",
            &good.id(),
            &FetchError::Tls(
                "TLS handshake aborted (connection reset or cut mid-negotiation).".into(),
            ),
        );
        assert!(pool.is_dead(&good.id()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The consequence, on a real pool: N fetches of hosts that do not
    // resolve must not retire the proxy lanes, because once every lane
    #[test]
    fn pool_pick_is_opt_in_and_never_for_crawl() {
        use crate::search::egress::EgressPool;
        use crate::transport::proxy::Proxy;
        let dir = std::env::temp_dir().join(format!("donsetch-pool-pick-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let pool = EgressPool::new(vec![Proxy::parse("http://127.0.0.1:24041").unwrap()]);
        assert!(
            pool_pick_enabled(None, true, true, Some(&pool)),
            "the opted-in fetch picks a lane"
        );
        assert!(
            !pool_pick_enabled(None, true, false, Some(&pool)),
            "off by default: rotate=false means no pick"
        );
        assert!(
            !pool_pick_enabled(None, false, true, Some(&pool)),
            "crawl owns its lane choice: never re-picked"
        );
        let explicit = Proxy::parse("http://127.0.0.1:24042").unwrap();
        assert!(
            !pool_pick_enabled(Some(&explicit), true, true, Some(&pool)),
            "an explicit lane mutes the pick"
        );
        assert!(
            !pool_pick_enabled(None, true, true, None),
            "no pool, no pick"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // is benched `pick_fetch(_, direct_ok=true)` hands back "direct"
    // and a rotation-configured fetch leaves on the real address.
    #[test]
    fn a_dead_target_name_does_not_bench_the_lane_or_fall_through_to_direct() {
        use crate::search::egress::EgressPool;
        use crate::transport::proxy::Proxy;
        let dir = std::env::temp_dir().join(format!("donsetch-lane-dns-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let proxy = Proxy::parse("http://127.0.0.1:34567").unwrap();
        let id = proxy.id();
        let pool = EgressPool::new(vec![proxy]);
        let lane = pool.pick_fetch("nope.invalid", true).unwrap();
        assert_eq!(lane.id, id, "a healthy proxy lane is picked first");

        for _ in 0..3 {
            note_lane_outcome(
                &pool,
                "nope.invalid",
                &id,
                &FetchError::Dns("could not resolve nope.invalid: no such host".into()),
            );
            note_lane_outcome(
                &pool,
                "nope.invalid",
                &id,
                &FetchError::DnsTimeout("the resolver did not answer within 5s".into()),
            );
        }
        assert!(
            !pool.is_dead(&id),
            "the origin's name is not the lane's fault"
        );
        assert_eq!(
            pool.pick_fetch("nope.invalid", true).map(|e| e.id),
            Some(id.clone()),
            "the lane stays on the proxy; direct is not reached"
        );
        // The lane's OWN connect failing is still a dead lane.
        note_lane_outcome(
            &pool,
            "nope.invalid",
            &id,
            &FetchError::Io(std::io::Error::other("connection refused")),
        );
        assert!(pool.is_dead(&id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A `direct` answer from the pool is not a pinned lane: it must
    // not mute HTTPS_PROXY. The client used to treat any pool answer
    // as a lane, so when the pool handed back `direct` (every lane
    // dead, or burned for this host) a configured env proxy was
    // silently skipped and the fetch left on the real address
    // (#302 review).
    #[test]
    fn a_direct_pool_answer_is_not_a_pinned_lane() {
        use crate::search::egress::Egress;
        use crate::transport::proxy::Proxy;
        let lane = Egress {
            id: "p1".into(),
            proxy: Some(Proxy::parse("http://127.0.0.1:24099").unwrap()),
        };
        let direct = Egress {
            id: "direct".into(),
            proxy: None,
        };
        assert!(pool_lane_is_proxy(Some(&lane)));
        assert!(!pool_lane_is_proxy(Some(&direct)));
        assert!(!pool_lane_is_proxy(None));
    }

    #[test]
    fn stealth_v3_bodyless_metadata_does_not_decode_a_missing_body() {
        for status in [204, 304] {
            for encoding in ["gzip", "br", "deflate", "zstd"] {
                let headers = vec![("content-encoding".into(), encoding.into())];
                let out = finish(
                    "http://owned.test/".into(),
                    "h1",
                    status,
                    headers.clone(),
                    Vec::new(),
                    false,
                )
                .expect("bodyless metadata must not start a decoder");
                assert_eq!(out.status, status);
                assert!(out.body.is_empty());
                assert_eq!(out.headers, headers);
            }
        }
        assert!(
            finish(
                "http://owned.test/".into(),
                "h1",
                200,
                vec![("content-encoding".into(), "gzip".into())],
                Vec::new(),
                false
            )
            .is_err(),
            "a truncated gzip 200 must still fail decoding"
        );
    }

    // The h3 lane used to hand back a literal Verdict::ContentOk for
    // any status >= 200: a Cloudflare challenge served over h3 (403 +
    // cf-mitigated) looked like clean content — no tier-2 escalation,
    // and compressed bodies skipped decompression entirely. Every
    // transport must leave through finish(), the one site of truth
    // for decompress + walls::detect. This pins the contract the h3
    // exit now rides.
    #[test]
    fn finish_scores_walls_and_decompresses_for_the_h3_exit() {
        let headers = vec![
            ("server".to_string(), "cloudflare".to_string()),
            ("cf-mitigated".to_string(), "challenge".to_string()),
        ];
        let out = finish(
            "https://walled.test/".into(),
            "h3",
            403,
            headers,
            b"<html><head><title>Just a moment...</title></head></html>".to_vec(),
            false,
        )
        .unwrap();
        assert!(
            matches!(out.verdict, Verdict::Challenge(_)),
            "a cf-mitigated 403 must classify as a challenge on every transport, got {:?}",
            out.verdict
        );
        assert_eq!(out.alpn, "h3");

        let mut gz = Vec::new();
        {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(b"<html><body>real content</body></html>")
                .unwrap();
            enc.finish().unwrap();
        }
        let out = finish(
            "https://ok.test/".into(),
            "h3",
            200,
            vec![("content-encoding".to_string(), "gzip".to_string())],
            gz,
            false,
        )
        .unwrap();
        assert!(
            out.body
                .windows(b"real content".len())
                .any(|w| w == b"real content"),
            "the h3 exit must decompress like every other transport"
        );
    }
}
