//! Fetch orchestrator with temporal stealth: origin pool, TLS session
//! resumption, persistent cookie jar, conditional revalidation cache,
//! Happy Eyeballs, single idempotent retry.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use url::Url;

use crate::detect::walls::{self, Verdict};
use crate::error::FetchError;
use crate::ghost::cache::CookieRecord;
use crate::profile::{BrowserProfile, RequestClass};
use crate::transport::pool::Pool;
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
}

impl Fetcher {
    /// Warm = a cached TLS session under `origin`: the repeat-navigation
    /// signal that flips TFO on at the TCP layer (Linux).
    fn sessions_has(&self, origin: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(origin)
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
        })
    }

    #[allow(dead_code)] // MCP surface will need this.
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
        self.fetch_via_jar_opts(url_str, None, false, None, true)
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
        self.fetch_via_jar_opts(url_str, proxy, use_jar, referer, false)
            .await
    }

    /// Full-knobs variant: `skip_cache` bypasses the revalidation
    /// cache entirely (probe path only; everything else keeps it).
    pub async fn fetch_via_jar_opts(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        skip_cache: bool,
    ) -> Result<FetchOutcome, FetchError> {
        // Centralized URL safety gate (fetch tier). The synchronous
        // literal checks run here (scheme, credentials, localhost and
        // private literals: no dial can follow a cached return). The
        // async DNS-aware tier runs exactly once per request, inside
        // fetch_once_via; this outer gate used to resolve DNS too,
        // doubling resolver RTT and load on every fetch (L7).
        crate::fetch::guards::validate_url_basic(url_str)?;
        let started = Instant::now();

        // Fresh-window cache hit: no request at all (browser-true).
        // Probes (v4 phase 0.2) skip this: a cached page is not
        // evidence about the wall RIGHT NOW.
        let check = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if skip_cache {
                CacheCheck::None
            } else {
                cache.check(url_str)
            }
        };
        let conditional = match check {
            CacheCheck::Fresh(body, status, headers) => {
                // Honest verdict on the cached body: a challenge page
                // that slipped into the cache must not be re-served
                // as ContentOk (only non-walls are stored, this is
                // defense in depth for pre-fix entries).
                let verdict = walls::detect(status, &headers, &body);
                return Ok(FetchOutcome {
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
            CacheCheck::Revalidate(cond) => cond,
            CacheCheck::None => Vec::new(),
        };

        let mut current = url_str.to_string();
        let mut redirects = 0u8;
        let mut first_request = true;

        // Resolve env-var proxy (HTTP_PROXY/HTTPS_PROXY/ALL_PROXY)
        // when no explicit proxy lane is passed. This follows the
        // curl/wget convention so users can route all DonSeTch
        // traffic through a proxy with a single env var. Re-resolved
        // for the CURRENT url at every hop (curl parity, E15: a
        // redirect to a NO_PROXY-covered host dials direct instead of
        // riding the env proxy for the rest of the chain). Explicit
        // proxy lanes stay pinned for the whole chain by design.
        // Proxies are NOT used for single-URL fetch by default:
        // one request to one URL does not rate-limit, and routing
        // through a proxy wastes bandwidth and hurts the TLS
        // fingerprint (residential proxies don't use our Chrome-true
        // BoringSSL stack). Proxies belong on search (many engines)
        // and crawl (many pages, same host) where rate limits bite.

        loop {
            let host = host_of(&current)?;
            let env_proxy = if proxy.is_none() && !crate::config::env_flag("DONSETCH_NO_ENV_PROXY")
            {
                crate::transport::proxy::from_env_for(&current)
            } else {
                None
            };
            let effective_proxy = proxy.or(env_proxy.as_ref());
            // Referer applies to the initial request only.
            // Redirects get no referer (avoids cross-origin leak).
            let ref_arg = if first_request { referer } else { None };
            // Revalidation conditionals were minted for the ORIGINAL
            // url's cache entry. Carrying them onto redirect hops lets
            // a colliding ETag on the target produce a false 304 and
            // merge the wrong cached body (B2). Only the first hop
            // sends them.
            let hop_conditional: &[(String, String)] =
                if first_request { &conditional } else { &[] };
            let mut out = self
                .fetch_once_via(&current, hop_conditional, effective_proxy, use_jar, ref_arg)
                .await?;
            {
                let mut jar = self
                    .jar
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let current_is_https =
                    Url::parse(&current).is_ok_and(|u| u.scheme().eq_ignore_ascii_case("https"));
                jar.store_from_headers(&host, &out.headers, current_is_https);
            }

            // 304: merge body from cache.
            if out.status == 304
                && let Some((body, status, headers)) = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .stored(&current)
            {
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
                        cache.store(&current, out.status, &out.headers, &out.body);
                    }

                    // Wall pushed back. If it left a cookie, do ONE
                    // cookie-warm retry (JS-less cookie walls pass on the
                    // second, cookie-carrying request).
                    if let Verdict::Challenge(_) = out.verdict
                        && header_value(&out.headers, "set-cookie").is_some()
                        && let Ok(mut retry) = self
                            .fetch_once_via(&current, &[], effective_proxy, use_jar, ref_arg)
                            .await
                    {
                        {
                            let mut jar = self
                                .jar
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let current_is_https = Url::parse(&current)
                                .is_ok_and(|u| u.scheme().eq_ignore_ascii_case("https"));
                            jar.store_from_headers(&host, &retry.headers, current_is_https);
                        }
                        retry.verdict = walls::detect(retry.status, &retry.headers, &retry.body);
                        if matches!(retry.verdict, Verdict::ContentOk) {
                            let mut cache = self
                                .cache
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            cache.store(&current, retry.status, &retry.headers, &retry.body);
                        }
                        out = retry;
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
            Some(p) => format!("{}|{}", p.id(), authority),
            None => authority.clone(),
        };

        // Header set from profile (Chrome order, coherence) + cookie + conditionals.
        let mut req_headers = self.profile.h1_headers_for_class(&authority, &path, class);
        if use_jar {
            let jar = self
                .jar
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cookie) = jar.header_for(host, &path, is_https) {
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
            let site = sec_fetch_site(ref_url, url_str);
            if let Some(pos) = req_headers.iter().position(|(n, _)| n == "sec-fetch-site") {
                req_headers[pos].1 = site.into();
            }
            // Chrome puts Referer after Sec-Fetch-Dest, before
            // Accept-Encoding.
            let ref_val = referer_value(ref_url, url_str);
            let pos = req_headers
                .iter()
                .position(|(n, _)| n == "accept-encoding")
                .unwrap_or(req_headers.len());
            req_headers.insert(pos, ("referer".into(), ref_val));
        }

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

        // 1) Try a pooled h2 connection for this origin.
        let pooled = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_h2(&origin);
        if let Some(mut conn) = pooled {
            match self
                .h2_request(&mut conn, &authority, &path, &req_headers, true)
                .await
            {
                Ok(out) => {
                    // verdict already scored by finish()
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
                    return Ok(out);
                }
                Err(e) => {
                    last_err = e;
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }
        Err(last_err)
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
        // Dial: https through an HTTP proxy goes through a CONNECT
        // tunnel; plaintext http:// through an HTTP proxy goes RAW
        // with an absolute-form request line (RFC 9112 3.2.2) —
        // CONNECT is for https only. SOCKS5 tunnels both; direct
        // dials use Happy Eyeballs.
        let tcp = match proxy {
            Some(p) if !is_https && p.is_http_connect() => p.connect_tcp().await?,
            Some(p) => p.connect(host, port).await?,
            None => tcp::happy_connect_with(host, port, self.sessions_has(origin)).await?,
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

        let session_key = match proxy {
            Some(p) => format!("{}|{}", p.id(), host),
            None => host.to_string(),
        };
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
                &session_key,
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
        .find(|(n, _)| n == "content-encoding")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let body = decompress::decompress(&encoding, &body)?;
    // Wall classification lives here: every caller used to score the
    // finished outcome with walls::detect right after the call; one
    // site of truth instead of N re-detections (Q4).
    let verdict = walls::detect(status, &headers, &body);
    Ok(FetchOutcome {
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

fn host_of(url_str: &str) -> Result<String, FetchError> {
    let url = url::Url::parse(url_str).map_err(|_| FetchError::InvalidUrl(url_str.into()))?;
    url.host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| FetchError::InvalidUrl(url_str.into()))
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
