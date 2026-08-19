//! Fetch orchestrator with temporal stealth: origin pool, TLS session
//! resumption, persistent cookie jar, conditional revalidation cache,
//! Happy Eyeballs, single idempotent retry.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::detect::walls::{self, Verdict};
use crate::error::FetchError;
use crate::ghost::cache::CookieRecord;
use crate::profile::BrowserProfile;
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
    sessions: tls::SessionStore,
    pool: Mutex<Pool>,
    jar: Mutex<CookieJar>,
    cache: Mutex<RevalidationCache>,
}

impl Fetcher {
    pub fn new(profile: BrowserProfile) -> Result<Self, FetchError> {
        let sessions = tls::new_session_store();
        let connector = tls::build_connector(&profile, sessions.clone())?;
        Ok(Self {
            profile,
            connector,
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
        let mut jar = self.jar.lock().unwrap();
        for c in cookies {
            jar.store_raw(&c.name, &c.value, &c.domain, c.expires_at);
        }
    }

    /// Export all cookies for a host with their expiry, for
    /// write-back to the persistent domain profile after a
    /// successful warm fetch.
    pub fn jar_snapshot(&self, host: &str) -> Vec<CookieRecord> {
        let jar = self.jar.lock().unwrap();
        jar.snapshot_for(host)
    }

    /// Fetch with browser-correct redirects, cookies, cache revalidation.
    pub async fn fetch(&self, url_str: &str) -> Result<FetchOutcome, FetchError> {
        self.fetch_via(url_str, None).await
    }

    /// Fetch through a specific egress lane (proxy). Redirects,
    /// cookies, revalidation all follow the lane — pool keys are
    /// proxy-scoped so egress IPs never share conns.
    pub async fn fetch_via(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar(url_str, proxy, true).await
    }

    /// Full lane control: `use_jar=false` keeps the shared cookie
    /// jar OUT of the request. Proxy lanes stay unlinked — the
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
        let started = Instant::now();

        // Fresh-window cache hit: no request at all (browser-true).
        let check = {
            let cache = self.cache.lock().unwrap();
            cache.check(url_str)
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
        // traffic through a proxy with a single env var. Resolved
        // once here and reused across redirect hops for consistency.
        let env_proxy = if proxy.is_none() {
            crate::transport::proxy::from_env_for(url_str)
        } else {
            None
        };
        let effective_proxy = proxy.or(env_proxy.as_ref());

        loop {
            let host = host_of(&current)?;
            // Referer applies to the initial request only.
            // Redirects get no referer (avoids cross-origin leak).
            let ref_arg = if first_request { referer } else { None };
            let mut out = self
                .fetch_once_via(&current, &conditional, effective_proxy, use_jar, ref_arg)
                .await?;
            {
                let mut jar = self.jar.lock().unwrap();
                jar.store_from_headers(&host, &out.headers);
            }

            // 304: merge body from cache.
            if out.status == 304
                && let Some((body, status, headers)) = self.cache.lock().unwrap().stored(&current)
            {
                out.status = status;
                out.headers = headers;
                out.body = body;
                out.cache = CacheState::Revalidated;
                // fetch_once_via already scored the bare 304, where
                // detect() sees an empty body and has no 3xx arm — it
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
                    let next = base
                        .join(&loc)
                        .map_err(|_| FetchError::Http(format!("bad redirect target: {loc}")))?;
                    if !matches!(next.scheme(), "http" | "https") {
                        // Non-web scheme (file://, ftp://, …):
                        // returned honestly, not followed.
                        out.elapsed = started.elapsed();
                        out.redirects = redirects;
                        return Ok(out);
                    }
                    // SSRF guard on EVERY hop: a public URL that
                    // redirects into a private network must not
                    // be followed (the initial-URL-only check is
                    // trivially bypassable otherwise).
                    if let Some(h) = next.host_str()
                        && crate::fetch::guards::is_ssrf_host(h)
                    {
                        return Err(FetchError::Http(format!(
                            "redirect to private/loopback address blocked: {h} — SSRF guard"
                        )));
                    }
                    current = next.to_string();
                }
                _ => {
                    out.verdict = walls::detect(out.status, &out.headers, &out.body);

                    // Only real content enters the revalidation cache.
                    // A challenge interstitial with an ETag would
                    // otherwise be re-served fresh as "content" on
                    // every later fetch (hardcoded ContentOk made it
                    // worse). Walls are never cacheable.
                    if matches!(out.verdict, Verdict::ContentOk) {
                        let mut cache = self.cache.lock().unwrap();
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
                            let mut jar = self.jar.lock().unwrap();
                            jar.store_from_headers(&host, &retry.headers);
                        }
                        retry.verdict = walls::detect(retry.status, &retry.headers, &retry.body);
                        if matches!(retry.verdict, Verdict::ContentOk) {
                            let mut cache = self.cache.lock().unwrap();
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
    /// `use_jar=false` keeps cookies out entirely — search
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
        let mut req_headers = self.profile.h1_headers(&authority);
        if use_jar {
            let jar = self.jar.lock().unwrap();
            if let Some(cookie) = jar.header_for(host, &path) {
                let pos = req_headers
                    .iter()
                    .position(|(n, _)| n == "priority")
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
        // `same-origin` or `cross-site` (never `none` — that's
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
                "invalid header value (CR/LF/NUL) — refused to send".into(),
            ));
        }

        // 1) Try a pooled h2 connection for this origin.
        let pooled = self.pool.lock().unwrap().take_h2(&origin);
        if let Some(mut conn) = pooled {
            match self
                .h2_request(&mut conn, &authority, &path, &req_headers, true)
                .await
            {
                Ok(mut out) => {
                    out.verdict = walls::detect(out.status, &out.headers, &out.body);
                    self.pool.lock().unwrap().put_h2(&origin, conn);
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
                Ok(mut out) => {
                    out.verdict = walls::detect(out.status, &out.headers, &out.body);
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
        let tcp = match proxy {
            Some(p) => p.connect(host, port).await?,
            None => tcp::happy_connect(host, port).await?,
        };

        // ── Plaintext http://: raw TCP straight into h1. ──
        // No h2 over plaintext (no browser does h2c); no TLS,
        // no session resumption, no ALPN.
        if !is_https {
            let mut stream = tcp;
            let resp =
                tokio::time::timeout(RESPONSE_TIMEOUT, h1::get(&mut stream, path, req_headers))
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
        let mut tls_stream = tokio::time::timeout(
            Duration::from_secs(15),
            tls::connect(
                &self.profile,
                &self.connector,
                host,
                tcp,
                &self.sessions,
                &session_key,
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
            self.pool.lock().unwrap().put_h2(origin, conn);
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
    Ok(FetchOutcome {
        url,
        status,
        alpn: alpn.into(),
        headers,
        body,
        redirects: 0,
        cache: CacheState::None,
        used_pool,
        verdict: Verdict::ContentOk,
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
/// everything else = `cross-site` (conservative — we don't
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
