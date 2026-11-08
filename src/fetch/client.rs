//! The fetcher every caller shares, over the v5 core (`src/fetch/v5.rs`).
//!
//! Two entry points:
//! - documents (`fetch`, `fetch_via*`, `fetch_persona*`, `fetch_cold_probe`)
//!   are one v5 encounter: identity order, page cache, data paths and the
//!   challenge engine, with learned state kept under the cache root;
//! - one-shot reads (`fetch_once_via*`) are one request with the caller's
//!   headers, for APIs, subresources and search endpoints.
//!
//! The app keeps what is the app's: egress lanes and their health, the
//! URL/SSRF guards (asked before the first request and before every redirect
//! hop), wall scoring and the public outcome surface. Cookies are the ones a
//! site set for an identity during a fetch; nothing is imported into them.

use std::time::Duration;

use crate::detect::walls::Verdict;
use crate::egress::proxy;
use crate::egress::request_route::RequestRoute;
use crate::error::FetchError;
use crate::profile::{BrowserProfile, RequestClass};

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
    /// `v5:<transport>:<identity>`: how the answer was read. Dev-only surface.
    pub alpn: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Redirect hops taken (v5 follows them itself; dev-only, 0).
    pub redirects: u8,
    pub cache: CacheState,
    /// Dev-only, always false: v5 owns connection pooling.
    pub used_pool: bool,
    pub verdict: Verdict,
    pub elapsed: Duration,
    /// The fetch layer read this body against a content contract and found
    /// content, not a wall: later bot-wall markers in it are passive scripts.
    pub verified: bool,
}

/// Pre-navigation redirect policy (v4.6): consulted for every
/// redirect hop BEFORE the target is dialed. Returning false stops
/// the chain — the redirect response itself becomes the outcome and
/// the caller records the honest skip — so an excluded or
/// robots-disallowed redirect target is never requested. Owned and
/// `'static`: callers clone in whatever the decision needs.
pub type RedirectGate = std::sync::Arc<
    dyn Fn(url::Url) -> futures_util::future::BoxFuture<'static, bool> + Send + Sync,
>;

pub struct Fetcher {
    profile: BrowserProfile,
    v5: crate::fetch::core::CoreFetcher,
    /// Shared egress fabric (v4 A2). When present and a proxy pool
    /// is configured, fetch sticks to one lane per host and rotates
    /// on 429/407/dead/timeout. None = historical env/slot path.
    egress: Option<std::sync::Arc<crate::search::egress::EgressPool>>,
}

impl Fetcher {
    pub fn new(profile: BrowserProfile) -> Result<Self, FetchError> {
        Ok(Self {
            profile,
            // `state.no_disk_state` keeps v5's learned state and page cache in
            // memory, like every other store.
            v5: crate::fetch::core::CoreFetcher::new(!crate::config::cfg().state.no_disk_state),
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

    /// Names of the cookies any identity holds for `host` (values stay inside
    /// the fetch layer).
    pub fn cookie_names(&self, host: &str) -> Vec<String> {
        self.v5.cookie_names(host)
    }

    pub async fn fetch(&self, url_str: &str) -> Result<FetchOutcome, FetchError> {
        self.fetch_via(url_str, None).await
    }

    /// Fetch through a specific egress lane (proxy). Redirects,
    /// cookies, revalidation all follow the lane.
    pub async fn fetch_via(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar(url_str, proxy, true).await
    }

    /// Full lane control: `use_jar=false` is a cold read: no learned state
    /// (cookies, identity order, page cache) is spent or kept.
    pub async fn fetch_via_jar(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_ref(url_str, proxy, use_jar, None).await
    }

    /// Evidence-grade cold probe: no learned state and no
    /// page cache (a cached page is not evidence about the wall RIGHT NOW).
    pub async fn fetch_cold_probe(&self, url_str: &str) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_opts(url_str, None, false, None, true, true, None)
            .await
    }

    /// Same as `fetch_via_jar` but with a referer header, marking
    /// the request link-initiated: `sec-fetch-site` is classified
    /// against the initiator. No referer → `none` (typed URL).
    pub async fn fetch_via_jar_ref(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_opts(url_str, proxy, use_jar, referer, false, true, None)
            .await
    }

    /// Full-knobs variant: `skip_cache` bypasses the page cache (a
    /// recrawl must see the live page). `pool_pick` gates the
    /// opt-in pool lane (`proxy.fetch_rotate`); crawl passes `false`
    /// because it owns its lane choice. `redirect_gate` is consulted for
    /// every redirect hop before the target is dialed (crawl scope/robots;
    /// `None` = follow freely).
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_via_jar_opts(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        skip_cache: bool,
        pool_pick: bool,
        redirect_gate: Option<RedirectGate>,
    ) -> Result<FetchOutcome, FetchError> {
        self.document(
            url_str,
            proxy,
            use_jar,
            referer,
            skip_cache,
            pool_pick,
            None,
            redirect_gate,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn document(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        skip_cache: bool,
        pool_pick: bool,
        route_override: Option<&RequestRoute>,
        redirect_gate: Option<RedirectGate>,
    ) -> Result<FetchOutcome, FetchError> {
        // Centralized URL safety gate: scheme, credentials, localhost and
        // private literals, before any route is chosen. The DNS-aware tier
        // runs once per request inside the v5 adapter.
        crate::fetch::guards::validate_url_basic(url_str)?;
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
        let pool_lane_key = route
            .pool_lane()
            .map(|(h, id)| (h.to_string(), id.to_string()));
        let hop_proxy = route.proxy_for(url_str)?;
        let out = self
            .v5
            .document(crate::fetch::core::Document {
                url: url_str,
                proxy: hop_proxy,
                route: route.clone(),
                learn: use_jar,
                skip_cache,
                referer,
                gate: redirect_gate,
            })
            .await;
        if let (Some(pool), Some((host, id))) = (&self.egress, &pool_lane_key) {
            match &out {
                Ok(out) if out.status == 429 => pool.note_fetch_rate_limited(host, id),
                Ok(out) if (200..300).contains(&out.status) => {
                    pool.report_ok(host, id);
                    pool.observe_rtt(id, out.elapsed);
                }
                Ok(_) => {}
                Err(e) => note_lane_outcome(pool, host, id, e),
            }
        }
        out
    }

    /// One cookie-less request with a request-local User-Agent: the plain
    /// identity, so no client hints or fetch metadata contradict the swapped
    /// UA. URL guards still run.
    pub async fn fetch_once_via_user_agent(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        user_agent: &str,
    ) -> Result<FetchOutcome, FetchError> {
        self.v5
            .once(crate::fetch::core::OneShot {
                url: url_str,
                conditional: &[],
                proxy,
                use_jar: false,
                referer: None,
                class: RequestClass::Navigation,
                user_agent: Some(user_agent),
            })
            .await
    }

    /// One request through `proxy`, or with `None` on the configured route,
    /// as a document read without a lane. `use_jar=false` keeps cookies out
    /// entirely: search engines get cookie-less requests so egress lanes stay
    /// unlinked.
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

    /// Class-aware variant: subresource reads carry the
    /// per-class header set on the wire.
    pub async fn fetch_once_via_class(
        &self,
        url_str: &str,
        conditional: &[(String, String)],
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        class: RequestClass,
    ) -> Result<FetchOutcome, FetchError> {
        self.v5
            .once(crate::fetch::core::OneShot {
                url: url_str,
                conditional,
                proxy,
                use_jar,
                referer,
                class,
                user_agent: None,
            })
            .await
    }

    /// The daemon's document read. Honors the fetch pool opt-in like every
    /// other fetch path (`proxy.fetch_rotate`).
    pub async fn fetch_persona(&self, url_str: &str) -> Result<FetchOutcome, FetchError> {
        self.fetch_persona_on_route(url_str, None).await
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

    /// A document read on a route chosen earlier in the same call (a related
    /// fallback URL keeps the lane the first read used).
    pub async fn fetch_persona_on_route(
        &self,
        url_str: &str,
        route: Option<&RequestRoute>,
    ) -> Result<FetchOutcome, FetchError> {
        self.document(url_str, None, true, None, false, true, route, None)
            .await
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

/// Schemeful same-site relationship for `sec-fetch-site` (Fetch
/// Metadata §2.3); the ordering is the chain fold's worst-wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SiteClass {
    SameOrigin,
    SameSite,
    CrossSite,
}

impl SiteClass {
    fn value(self) -> &'static str {
        match self {
            SiteClass::SameOrigin => "same-origin",
            SiteClass::SameSite => "same-site",
            SiteClass::CrossSite => "cross-site",
        }
    }
}

/// The "site" of a host: its registrable domain when it has one,
/// else the host itself (IPs, localhost, single labels).
fn site_of(host: &str) -> &str {
    psl::domain(host.as_bytes())
        .and_then(|domain| std::str::from_utf8(domain.as_bytes()).ok())
        .unwrap_or(host)
}

/// Chrome's `sec-fetch-site` relationship between the initiator and
/// a URL (schemeful): same-origin = scheme+host+port, same-site =
/// same scheme and same registrable domain (port ignored), else
/// cross-site. Unparseable input fails closed to cross-site.
fn classify_site(initiator: &str, target: &str) -> SiteClass {
    let (Ok(initiator), Ok(target)) = (url::Url::parse(initiator), url::Url::parse(target)) else {
        return SiteClass::CrossSite;
    };
    if initiator.scheme() == target.scheme()
        && initiator.host_str() == target.host_str()
        && initiator.port_or_known_default() == target.port_or_known_default()
    {
        return SiteClass::SameOrigin;
    }
    if initiator.scheme() == target.scheme()
        && site_of(initiator.host_str().unwrap_or("")) == site_of(target.host_str().unwrap_or(""))
    {
        return SiteClass::SameSite;
    }
    SiteClass::CrossSite
}

/// Chrome's `sec-fetch-site` value for a navigation from `initiator` to `target`.
pub(crate) fn fetch_site(initiator: &str, target: &str) -> &'static str {
    classify_site(initiator, target).value()
}

/// Chrome's default referrer policy `strict-origin-when-cross-origin`:
/// same-origin = full URL, cross-origin = origin only.
pub(crate) fn referer_value(referer: &str, target: &str) -> String {
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

/// Lane accounting for one failed hop on a pool lane. Shared by the
/// fetch path and the crawl adapter so both attribute errors the same
/// way: origin-side and local errors never bench a healthy lane.
pub(crate) fn note_lane_outcome(
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

/// `Dns`/`DnsTimeout` are the ORIGIN's name, never the lane's; a policy
/// refusal is about the URL, a protocol error about the origin and a
/// proxy configuration error about the configuration: none of these says
/// anything about the lane.
fn lane_note(e: &FetchError) -> Option<LaneNote> {
    let msg = e.to_string();
    match e {
        FetchError::Dns(_) | FetchError::DnsTimeout(_) => None,
        // A proxy URL that cannot be read is named in the message, host
        // included, so the text arms below would bench a lane on
        // "connect.example.net"; with no proxy lane left, pick_fetch hands
        // the host the direct egress.
        FetchError::InvalidUrl(_)
        | FetchError::Ssrf(_)
        | FetchError::Denied { .. }
        | FetchError::NonHttpRedirect(_)
        | FetchError::TooManyRedirects
        | FetchError::ProxyConfig(_) => None,
        FetchError::Timeout => Some(LaneNote::Timeout),
        // A certificate-verify failure is the certificate the far
        // side presented, not lane health. Egress-flavored TLS
        // failures ("handshake aborted" / "cut short") still bench.
        FetchError::Tls(msg) if crate::error::is_cert_verify_failure(msg) => {
            Some(LaneNote::OriginTls)
        }
        // The lane's proxy answered that it could not reach the target, which
        // is how a name it cannot resolve can come back: the lane connected,
        // so it stays seated, as for a name the local guard cannot resolve.
        FetchError::Io(error) if crate::fetch::core::proxy_refused_the_target(error) => None,
        FetchError::Io(_) | FetchError::Tls(_) => Some(LaneNote::Dead),
        // A proxy that demands credentials. Ahead of the text arms:
        // "CONNECT -> 407" must never read as a dead lane.
        _ if msg.contains("CONNECT -> 407") => Some(LaneNote::AuthFail),
        _ if msg.contains("timed out") => Some(LaneNote::Timeout),
        _ if msg.contains("connection") || msg.contains("connect") => Some(LaneNote::Dead),
        _ => None,
    }
}

#[cfg(test)]
mod fetcher_tests {
    use super::*;
    use crate::error::FetchError;

    #[test]
    fn stealth_v3_fetch_metadata_site_classification() {
        // Chrome's sec-fetch-site values: same-origin (scheme+host+port),
        // same-site (same scheme and registrable domain, port ignored),
        // cross-site otherwise (schemeful).
        assert_eq!(
            classify_site("https://www.example.com/app", "https://www.example.com/x"),
            SiteClass::SameOrigin
        );
        assert_eq!(
            classify_site(
                "https://www.example.com:8443/app",
                "https://www.example.com/x"
            ),
            SiteClass::SameSite
        );
        assert_eq!(
            classify_site("https://www.example.com/", "https://api.example.com/x"),
            SiteClass::SameSite
        );
        assert_eq!(
            classify_site("http://www.example.com/", "https://www.example.com/x"),
            SiteClass::CrossSite
        );
        assert_eq!(
            classify_site("https://www.example.com/", "https://www.example.org/x"),
            SiteClass::CrossSite
        );
        assert_eq!(
            classify_site("http://127.0.0.1:8080/", "http://127.0.0.1/x"),
            SiteClass::SameSite
        );
        assert_eq!(
            classify_site("http://127.0.0.1/", "http://127.0.0.2/x"),
            SiteClass::CrossSite
        );
        assert_eq!(
            classify_site("http://localhost/", "http://localhost:9000/x"),
            SiteClass::SameSite
        );
        // The chain fold keeps the worst relationship.
        assert_eq!(
            SiteClass::SameOrigin.max(SiteClass::CrossSite),
            SiteClass::CrossSite
        );
        assert_eq!(
            SiteClass::SameOrigin.max(SiteClass::SameSite),
            SiteClass::SameSite
        );
        assert_eq!(
            SiteClass::SameSite.max(SiteClass::CrossSite),
            SiteClass::CrossSite
        );
    }

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
        assert_eq!(
            lane_note(&FetchError::InvalidUrl(
                "http://connectivity.example:99999/".into()
            )),
            None
        );
        assert_eq!(lane_note(&FetchError::TooManyRedirects), None);
    }

    #[test]
    fn a_rule_denial_leaves_lane_health_alone() {
        assert_eq!(
            lane_note(&FetchError::Denied {
                rule: "connect.widgets.example".into(),
                message: "connection timed out here; CONNECT -> 407 is expected".into(),
                kind: "walled",
                reason: None,
                url: "https://connect.widgets.example/".into(),
            }),
            None
        );
        assert_eq!(lane_note(&FetchError::NonHttpRedirect("ftp".into())), None);
        assert_eq!(
            lane_note(&FetchError::Io(std::io::Error::other("connection refused"))),
            Some(LaneNote::Dead)
        );
    }

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

    #[test]
    fn a_bad_origin_certificate_moves_the_host_without_benching_the_lane() {
        use crate::search::egress::EgressPool;
        let dir = std::env::temp_dir().join(format!("donsetch-lane-cert-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let bad = proxy::Proxy::parse("http://127.0.0.1:34568").unwrap();
        let good = proxy::Proxy::parse("http://127.0.0.1:34569").unwrap();
        let pool = EgressPool::new(vec![bad.clone(), good.clone()]);
        let cert_fail = FetchError::Tls(
            "TLS certificate verification failed. The presentation cert chain was not issued by any trusted root."
                .into(),
        );
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
        note_lane_outcome(&pool, "self-signed.example", &bad.id(), &cert_fail);
        assert!(!pool.is_dead(&bad.id()));
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

    #[test]
    fn pool_pick_is_opt_in_and_never_for_crawl() {
        use crate::search::egress::EgressPool;
        let dir = std::env::temp_dir().join(format!("donsetch-pool-pick-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let pool = EgressPool::new(vec![proxy::Proxy::parse("http://127.0.0.1:24041").unwrap()]);
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
        let explicit = proxy::Proxy::parse("http://127.0.0.1:24042").unwrap();
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

    #[test]
    fn a_dead_target_name_does_not_bench_the_lane_or_fall_through_to_direct() {
        use crate::search::egress::EgressPool;
        let dir = std::env::temp_dir().join(format!("donsetch-lane-dns-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let p = proxy::Proxy::parse("http://127.0.0.1:34567").unwrap();
        let id = p.id();
        let pool = EgressPool::new(vec![p]);
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
        note_lane_outcome(
            &pool,
            "nope.invalid",
            &id,
            &FetchError::Io(std::io::Error::other("connection refused")),
        );
        assert!(pool.is_dead(&id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_direct_pool_answer_is_not_a_pinned_lane() {
        use crate::search::egress::Egress;
        let lane = Egress {
            id: "p1".into(),
            proxy: Some(proxy::Proxy::parse("http://127.0.0.1:24099").unwrap()),
        };
        let direct = Egress {
            id: "direct".into(),
            proxy: None,
        };
        assert!(pool_lane_is_proxy(Some(&lane)));
        assert!(!pool_lane_is_proxy(Some(&direct)));
        assert!(!pool_lane_is_proxy(None));
    }

    async fn owned_status_fetch(status: u16) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!("donsetch-owned-fetch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.proxy.fetch_rotate = true;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/owned-status", origin.local_addr().unwrap());
        let p = proxy::Proxy::parse(&format!("http://{}", proxy_listener.local_addr().unwrap()))
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
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Owned\r\nContent-Length: 5\r\nConnection: close\r\n\r\nowned"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let pool = std::sync::Arc::new(crate::search::egress::EgressPool::new(vec![p.clone()]));
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default())
            .unwrap()
            .with_egress(std::sync::Arc::clone(&pool));
        let response = fetcher.fetch(&url).await.unwrap();
        server.await.unwrap();
        assert_eq!(response.status, status);
        assert_eq!(response.body, b"owned");
        assert_eq!(
            response.route.proxy_for(&url).unwrap().unwrap().id(),
            p.id()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), origin.accept())
                .await
                .is_err(),
            "a pooled success must not reach direct"
        );
        assert!(
            pool.rtt_ms(&p.id()).is_some(),
            "HTTP{status} must teach the selected pool lane RTT"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stealth_v3_success_status_201_updates_fetch_pool_health() {
        owned_status_fetch(201).await;
    }

    #[tokio::test]
    async fn stealth_v3_success_status_200_retains_fetch_pool_health() {
        owned_status_fetch(200).await;
    }

    /// A proxy URL v5 cannot read is the configuration, not the lane: the
    /// error names the proxy's host ("connect.example.net"), and the text arms
    /// must not read that as a dead lane, or the next read would fall to the
    /// direct egress. A lane that refuses the connection is still benched.
    #[test]
    fn a_proxy_configuration_error_is_not_lane_health() {
        for host in ["proxy.example.net", "connect.example.net"] {
            let error = FetchError::ProxyConfig(format!(
                "cannot read proxy URL \"socks5h://***@{host}:1080\": invalid port number"
            ));
            assert_eq!(lane_note(&error), None, "{error}");
        }
        let refused = FetchError::Io(std::io::Error::other("connection refused"));
        assert!(lane_note(&refused).is_some(), "{refused}");
    }

    // In-process loopback mock: the runtime hosting the mock must keep
    // polling while the fetch is pending.
    fn loopback_config() {
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
    }

    async fn loopback_mock() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/owned-status", origin.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(socket.read_u8().await.unwrap());
            }
            assert!(head.starts_with(b"GET /owned-status HTTP/1.1\r\n"));
            let body = "<article><p>owned</p></article>";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 Owned\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        (url, h)
    }

    // v5 runs synchronously on a blocking thread; the runtime hosting an
    // in-process loopback peer must keep polling while the fetch is pending
    // (an inline call would deadlock every same-runtime mock).
    #[tokio::test]
    async fn a_one_shot_read_keeps_the_loopback_peer_polled() {
        loopback_config();
        let (url, mock) = loopback_mock().await;
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            fetcher.fetch_once_via(&url, &[], None, true, None),
        )
        .await
        .expect("the wire bridge must not starve the runtime")
        .expect("one-shot loopback read");
        assert_eq!(out.status, 200);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), mock).await;
    }

    // v4.6: a redirect hop refused by the per-request policy must
    // never be dialed — the chain stops at the redirect response.
    #[tokio::test]
    async fn redirect_gate_refuses_before_the_target_is_dialed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir =
            std::env::temp_dir().join(format!("donsetch-redirect-gate-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = origin.local_addr().unwrap().port();
        let target = format!("http://127.0.0.1:{port}/target");
        let start = format!("http://127.0.0.1:{port}/start");
        let hits: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let h = std::sync::Arc::clone(&hits);
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = origin.accept().await {
                let mut head = Vec::new();
                loop {
                    let mut b = [0u8; 512];
                    let n = socket.read(&mut b).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&b[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&head);
                let path = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                h.lock().unwrap().push(path.clone());
                let response = if path == "/start" {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nowned"
                        .to_string()
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let gate: RedirectGate = std::sync::Arc::new(|_url: url::Url| Box::pin(async { false }));
        let out = fetcher
            .fetch_via_jar_opts(&start, None, false, None, true, false, Some(gate))
            .await
            .unwrap();
        assert_eq!(out.status, 302, "the redirect response returns honestly");
        assert_eq!(out.url, start);
        server.abort();
        let seen = hits.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec!["/start".to_string()],
            "the refused target must never be dialed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A PDF must arrive byte-exact, or DonSheet reads mojibake: both a
    // `.pdf` URL and a PDF answer on a URL that does not look like one.
    #[tokio::test]
    async fn pdf_documents_keep_their_bytes_exact() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loopback_config();
        static PDF_BODY: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer<< /Root 1 0 R >>\n%%EOF\n";
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", origin.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = origin.accept().await {
                let mut head = Vec::new();
                loop {
                    let mut b = [0u8; 512];
                    let n = socket.read(&mut b).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&b[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let mut resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    PDF_BODY.len()
                )
                .into_bytes();
                resp.extend_from_slice(PDF_BODY);
                socket.write_all(&resp).await.unwrap();
            }
        });
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let shaped = fetcher.fetch(&format!("{base}/paper.pdf")).await.unwrap();
        assert_eq!(shaped.status, 200);
        assert_eq!(
            shaped.body, PDF_BODY,
            "a .pdf URL must arrive byte-exact (lossy text decode would alter the bytes)"
        );
        let surprise = fetcher.fetch(&format!("{base}/download")).await.unwrap();
        assert_eq!(surprise.status, 200);
        assert_eq!(
            surprise.body, PDF_BODY,
            "a PDF answer on a non-pdf URL must arrive byte-exact"
        );
        server.abort();
    }
}
