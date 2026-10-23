//! Fetch orchestrator over the v5 engine.
//!
//! One wire, two entry points (the engine's construction contract):
//! - documents (`fetch`, `fetch_via*`, `fetch_persona*`) go through
//!   `Engine::fetch_with` — the cache, the JS runtime and the learned hosts
//!   live there;
//! - one-shot API/subresource reads (`fetch_once_via*`, `fetch_cold_probe`)
//!   go through the engine's `Wire` with the per-request knobs (egress,
//!   jar suppression, raw bytes, redirects re-gated per hop).
//!
//! The app keeps what is the app's: egress lanes and their health, the
//! URL/SSRF guards, wall scoring, the public outcome surface, and the crawl
//! hop loop (an async scope/robots gate must refuse BEFORE a hop is dialed,
//! which the sync redirect consult cannot await — that consult carries the
//! rules/SSRF gate for every engine-followed hop).
//!
//! The app's jar IS the engine's jar now — one store under
//! `DONSETCH_CACHE_DIR` — and the vault flows (session replay, warm
//! handoff, login/logout rebuilds) sync records through `Jar::upsert` /
//! `Jar::remove_domains`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use doncore::engine::{CacheState as EngineCacheState, FetchOpts, JsMode};
use doncore::wire::{HopConsult, HopDecision, ReqKind, WireReq, WireResp};

use crate::detect::walls::{self, Verdict};
use crate::egress::proxy;
use crate::egress::request_route::RequestRoute;
use crate::error::FetchError;
use crate::ghost::cache::CookieRecord;
use crate::profile::{BrowserProfile, RequestClass};

const MAX_REDIRECTS: u8 = 10;

/// The engine's wall budget for one document fetch (`Engine::fetch_with`):
/// the corpus' few-seconds ceiling. One-shot wire reads carry no budget of
/// their own; their callers wrap them in timeouts.
const DOC_BUDGET_MS: u64 = 25_000;

/// The engine's identity persona. `"154"` is the ClientHello-parity class
/// measured against the installed Chromium; anything else is the stock
/// preset. The swap keeps the product on the measured class.
const ENGINE_PERSONA: &str = "154";

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
    /// The engine's route label for documents (`"wire"`, `"wire+js"`,
    /// `"wire+clearance"`), `"wire"` for one-shot reads. Dev-only surface.
    pub alpn: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Redirect hops taken. The engine owns the chain on the document path,
    /// so this is populated only on the app's own hop loop (dev-only field).
    pub redirects: u8,
    pub cache: CacheState,
    /// True when the request rode a pooled (pre-existing) connection. The
    /// engine owns pooling now; dev-only field, always false.
    pub used_pool: bool,
    pub verdict: Verdict,
    pub elapsed: Duration,
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
    /// The one engine: identity, wire, state plane (cache / warm jar /
    /// learned hosts) under `state_root()`. Shared behind an `Arc` so the
    /// synchronous wire calls can bridge onto a blocking thread without
    /// moving the `Fetcher`.
    engine: std::sync::Arc<doncore::engine::Engine>,
    /// Domains currently fed from the session vault (the last rewrite).
    /// A vault rewrite drops only these: warm solve cookies from the
    /// engine's own pipeline are never collateral.
    vault_domains: Mutex<std::collections::HashSet<String>>,
    /// Shared egress fabric (v4 A2). When present and a proxy pool
    /// is configured, fetch sticks to one lane per host and rotates
    /// on 429/407/dead/timeout. None = historical env/slot path.
    egress: Option<std::sync::Arc<crate::search::egress::EgressPool>>,
}

impl Fetcher {
    pub fn new(profile: BrowserProfile) -> Result<Self, FetchError> {
        #[cfg(test)]
        crate::test_support::sandbox_state_dir();
        let engine = std::sync::Arc::new(doncore::client::open(
            ENGINE_PERSONA,
            Vec::new(),
            doncore::client::Features::default(),
        ));
        Ok(Self {
            profile,
            engine,
            vault_domains: Mutex::new(std::collections::HashSet::new()),
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

    // ── the jar bridge ──────────────────────────────────────────────────
    //
    // The engine jar is THE jar: one store that the wire, the JS runtime
    // and these vault flows all share. A CookieRecord → Cookie conversion
    // keeps name/value/domain/expiry/Secure; Path/SameSite cannot be
    // represented in the flat store (Recorded limitations, status.md).

    fn jar(&self) -> std::sync::MutexGuard<'_, doncore::wire::Jar> {
        self.engine
            .wire
            .jar
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn engine_cookie(rec: &CookieRecord) -> doncore::wire::Cookie {
        let now = doncore::state::now_secs();
        doncore::wire::Cookie {
            name: rec.name.clone(),
            value: rec.value.clone(),
            domain: rec.domain.clone(),
            secure: rec.secure,
            expires: rec.expires_at,
            created: now,
            updated: now,
        }
    }

    fn record_of(c: &doncore::wire::Cookie) -> CookieRecord {
        CookieRecord {
            name: c.name.clone(),
            value: c.value.clone(),
            domain: c.domain.clone(),
            path: "/".into(),
            expires_at: c.expires,
            secure: c.secure,
            http_only: false,
            same_site: "lax".into(),
        }
    }

    /// Import cookies harvested by DonGhost (tier-2 solve) into the
    /// shared jar so the next fetch carries the clearance.
    pub async fn import_cookies(&self, cookies: &[CookieRecord]) {
        let list: Vec<_> = cookies.iter().map(Self::engine_cookie).collect();
        self.jar().upsert(list);
    }

    /// Import VAULT-backed cookies (session harvests, the boot vault
    /// replay): as `import_cookies`, and the domains register as
    /// vault-fed so a login/logout jar rebuild really clears them. The
    /// ledger only ever grows here — an empty replay must not wipe what a
    /// previous session's vault owned.
    pub async fn import_vault_cookies(&self, cookies: &[CookieRecord]) {
        let list: Vec<_> = cookies.iter().map(Self::engine_cookie).collect();
        self.jar().upsert(list);
        let mut vault = self.vault_domains.lock().unwrap_or_else(|p| p.into_inner());
        for c in cookies {
            if let Some(d) = normalize_domain(&c.domain) {
                vault.insert(d);
            }
        }
        let stored = load_vault_domains();
        let mut ledger = stored.clone();
        ledger.extend(vault.iter().cloned());
        if ledger != stored {
            save_vault_domains(&ledger);
        }
    }

    /// Replace the vault-sourced subset (login or logout just happened on
    /// disk). Only domains the vault owns are dropped: warm solve cookies
    /// from other domains survive untouched. Ownership survives restarts
    /// through the ledger beside the jar — a fresh process replaying an
    /// EMPTY vault must still clear the previous session's vault cookies,
    /// or a logged-out login would ride the next boot.
    pub async fn reset_to(&self, cookies: &[CookieRecord]) {
        let stored = load_vault_domains();
        let mut touched: std::collections::HashSet<String> = stored.clone();
        touched.extend(
            self.vault_domains
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .cloned(),
        );
        let mut incoming: std::collections::HashSet<String> = Default::default();
        for c in cookies {
            if let Some(d) = normalize_domain(&c.domain) {
                touched.insert(d.clone());
                incoming.insert(d);
            }
        }
        let domains: Vec<String> = touched.into_iter().collect();
        self.jar().remove_domains(&domains);
        let list: Vec<_> = cookies.iter().map(Self::engine_cookie).collect();
        self.jar().upsert(list);
        *self.vault_domains.lock().unwrap_or_else(|p| p.into_inner()) = incoming.clone();
        if incoming != stored {
            save_vault_domains(&incoming);
        }
    }

    /// Whole-jar export for the tier-1 cookie vault (v4 phase 1.4): the
    /// browser cookie store view that makes a returning agent replay like
    /// a returning device across process restarts.
    pub async fn jar_all_snapshot(&self) -> Vec<CookieRecord> {
        let now = doncore::state::now_secs();
        let jar = self.jar();
        jar.cookies
            .iter()
            .filter(|c| c.expires.map(|e| e > now).unwrap_or(true))
            .map(Self::record_of)
            .collect()
    }

    /// Export all cookies for a host with their expiry, for write-back to
    /// the persistent domain profile after a successful warm fetch.
    pub fn jar_snapshot(&self, host: &str) -> Vec<CookieRecord> {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        let now = doncore::state::now_secs();
        let jar = self.jar();
        jar.cookies
            .iter()
            .filter(|c| c.expires.map(|e| e > now).unwrap_or(true))
            .filter(|c| {
                let d = c.domain.to_ascii_lowercase();
                !d.is_empty() && (d == host || host.ends_with(&format!(".{d}")))
            })
            .map(Self::record_of)
            .collect()
    }

    // ── request identity ────────────────────────────────────────────────

    /// The app-side extras layered over the engine's identity: a persona
    /// accept-language, a legacy User-Agent, the referer/`sec-fetch-site`
    /// pair, conditionals. The engine merges request headers over its
    /// emulation: the same name wins from the request.
    fn extra_headers(
        target: &str,
        conditional: &[(String, String)],
        referer: Option<&str>,
        site: Option<&'static str>,
        identity: RequestIdentity<'_>,
        profile_ua: &str,
        profile_sec_ch_ua: &str,
    ) -> Vec<(String, String)> {
        let mut h: Vec<(String, String)> = Vec::new();
        if let Some(al) = identity.accept_language.filter(|al| !al.is_empty()) {
            h.push(("accept-language".to_owned(), al.to_owned()));
        }
        // The profile owns the User-Agent: the engine's emulation carries
        // the engine persona's own version, which must not leak through
        // when the product profile runs a different one. A legacy UA
        // (request-local lane) still wins here.
        h.push((
            "user-agent".to_owned(),
            identity.legacy_user_agent.unwrap_or(profile_ua).to_owned(),
        ));
        // Client hints ride with the profile too (the ghost and tier 1
        // must claim the same brand/version list); a legacy identity
        // carries no hints at all.
        if identity.legacy_user_agent.is_none() {
            h.push(("sec-ch-ua".to_owned(), profile_sec_ch_ua.to_owned()));
        }
        // The chain fold keeps the worst sec-fetch-site seen (Fetch Metadata
        // §2.3): a link-initiated hop derives it from the initiator, later
        // hops carry the worst so far. One value, never a duplicate header.
        if let Some(sev) = site.or_else(|| referer.map(|r| classify_site(r, target).value())) {
            h.push(("sec-fetch-site".to_owned(), sev.to_owned()));
        }
        if let Some(ref_url) = referer {
            h.push(("referer".to_owned(), referer_value(ref_url, target)));
        }
        h.extend(conditional.iter().cloned());
        h
    }

    // ── the two wire entry points ───────────────────────────────────────

    /// One request through the engine's wire with the product's knobs.
    #[allow(clippy::too_many_arguments)]
    async fn wire_once(
        &self,
        url_str: &str,
        kind: ReqKind,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        extra_headers: Vec<(String, String)>,
        follow_redirects: bool,
        legacy_identity: bool,
    ) -> Result<FetchOutcome, FetchError> {
        // Centralized gate: credentials/host checks and the DNS-aware SSRF
        // tier run before any dial.
        crate::fetch::guards::ensure_url_safe(url_str).await?;
        // Values synthesized from responses (referer, cookies) must never
        // split the request line.
        if extra_headers.iter().any(|(n, v)| {
            !crate::fetch::guards::valid_header_value(n)
                || !crate::fetch::guards::valid_header_value(v)
        }) {
            return Err(FetchError::Http(
                "invalid header value (CR/LF/NUL) : refused to send".into(),
            ));
        }
        let started = Instant::now();
        let refusal: std::sync::Arc<Mutex<Option<FetchError>>> = Default::default();
        let req = WireReq {
            method: "GET".into(),
            url: url_str.to_string(),
            kind,
            extra_headers,
            body: None,
            bytes: true,
            no_cookies: !use_jar,
            proxy: proxy.map(proxy::Proxy::wire_url),
            follow_redirects: if follow_redirects { None } else { Some(false) },
            legacy_identity,
            hop_consult: Some(hop_consult(std::sync::Arc::clone(&refusal))),
            ..Default::default()
        };
        let resp = match wire_call_cancel(self.engine.wire.clone(), req).await {
            Ok(resp) => resp,
            Err(raw) => return Err(prefer_refusal(raw, &refusal)),
        };
        let mut out = self.wire_outcome(resp);
        out.route = proxy
            .cloned()
            .map(RequestRoute::pinned)
            .unwrap_or_else(RequestRoute::direct);
        out.elapsed = started.elapsed();
        Ok(out)
    }

    fn wire_outcome(&self, resp: WireResp) -> FetchOutcome {
        let body = resp.raw.unwrap_or_else(|| resp.body.clone().into_bytes());
        let verdict = walls::detect(resp.status, &resp.headers, &body);
        FetchOutcome {
            route: RequestRoute::direct(),
            url: resp.final_url,
            status: resp.status,
            alpn: "wire".into(),
            headers: resp.headers,
            body,
            redirects: 0,
            cache: CacheState::None,
            used_pool: false,
            verdict,
            elapsed: Duration::ZERO,
        }
    }

    /// One document fetch through the engine pipeline (cache, JS, learned
    /// hosts). The guard runs on the initial URL; every redirect hop the
    /// engine follows is re-gated through the hop consult BEFORE its dial.
    /// A PDF-shaped or PDF-answering URL takes the raw wire instead: the
    /// engine's document path decodes bodies as text, and DonSheet needs
    /// the bytes verbatim.
    async fn doc_fetch(
        &self,
        url_str: &str,
        proxy: Option<proxy::Proxy>,
        use_jar: bool,
        route: RequestRoute,
        extra: Vec<(String, String)>,
    ) -> Result<FetchOutcome, FetchError> {
        crate::fetch::guards::ensure_url_safe(url_str).await?;
        let started = Instant::now();
        // The URL shape alone decides the easy case: no text pipeline for
        // a PDF (arXiv serves /pdf/1706.03762 with no extension).
        if crate::fetch::guards::is_pdf_url_like(url_str) {
            return self
                .raw_document_fetch(url_str, proxy, use_jar, extra, route, started)
                .await;
        }
        let refusal: std::sync::Arc<Mutex<Option<FetchError>>> = Default::default();
        let raw_extra = extra.clone();
        let opts = FetchOpts {
            proxy: proxy.as_ref().map(proxy::Proxy::wire_url),
            no_cookies: !use_jar,
            extra_headers: extra,
            hop_consult: Some(hop_consult(std::sync::Arc::clone(&refusal))),
        };
        let engine = std::sync::Arc::clone(&self.engine);
        let url_owned = url_str.to_string();
        let out = match wire_call(move || {
            engine.fetch_with(&url_owned, JsMode::Auto, DOC_BUDGET_MS, &opts)
        })
        .await
        {
            Ok(out) => out,
            Err(raw) => return Err(prefer_refusal(raw, &refusal)),
        };
        // A 304 that reaches the app is never a valid revalidation — the
        // engine consumes those. It is an unsolicited or redirected answer
        // with no matching validator snapshot, and serving it would be an
        // empty wall.
        if out.status == 304 {
            return Err(FetchError::Http(
                "304 response without a matching request validator snapshot".into(),
            ));
        }
        // A URL that did not LOOK like a PDF answered as one: the engine
        // already decoded the bytes as text, so read them again raw.
        let ct = out
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        if crate::fetch::guards::is_pdf(out.html.as_bytes(), ct) {
            return self
                .raw_document_fetch(url_str, proxy, use_jar, raw_extra, route, started)
                .await;
        }
        // The engine's `html` is the final document (post-JS when JS ran);
        // a non-HTML body lands there verbatim too.
        let body: Vec<u8> = if !out.html.is_empty() {
            out.html.into_bytes()
        } else {
            out.text.into_bytes()
        };
        let verdict = walls::detect(out.status, &out.headers, &body);
        Ok(FetchOutcome {
            route,
            url: out.url,
            status: out.status,
            alpn: out.label,
            headers: out.headers,
            body,
            redirects: 0,
            cache: Self::app_cache(out.cache),
            used_pool: false,
            verdict,
            elapsed: started.elapsed(),
        })
    }

    /// One raw wire read for a document-shaped binary: the engine's text
    /// decode never sees it, so the bytes stay exact (DonSheet reads them
    /// verbatim). Same guards, same hop consult, same jar knobs as the
    /// text path; never cache-served (a raw read is fresh by definition).
    async fn raw_document_fetch(
        &self,
        url_str: &str,
        proxy: Option<proxy::Proxy>,
        use_jar: bool,
        extra: Vec<(String, String)>,
        route: RequestRoute,
        started: Instant,
    ) -> Result<FetchOutcome, FetchError> {
        let mut out = self
            .wire_once(
                url_str,
                ReqKind::Document,
                proxy.as_ref(),
                use_jar,
                extra,
                true,
                false,
            )
            .await?;
        out.route = route;
        out.elapsed = started.elapsed();
        Ok(out)
    }

    fn app_cache(state: Option<doncore::engine::CacheOutcome>) -> CacheState {
        match state.map(|c| c.state) {
            Some(EngineCacheState::Fresh) => CacheState::Fresh,
            Some(EngineCacheState::Revalidated) => CacheState::Revalidated,
            _ => CacheState::None,
        }
    }

    /// The app's own redirect loop, for paths that carry a `RedirectGate`
    /// (crawl scope/robots): each hop is a single request
    /// (`follow_redirects: Some(false)`), validated and gated BEFORE the
    /// next target is dialed. Also the honest-return path for non-http(s)
    /// redirect targets.
    #[allow(clippy::too_many_arguments)]
    async fn hop_loop(
        &self,
        url_str: &str,
        proxy: Option<proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        identity: RequestIdentity<'_>,
        redirect_gate: RedirectGate,
        route: RequestRoute,
        pool_lane: Option<(String, String)>,
    ) -> Result<FetchOutcome, FetchError> {
        crate::fetch::guards::validate_url_basic(url_str)?;
        let started = Instant::now();
        let mut current = url_str.to_string();
        let mut redirects = 0u8;
        let mut first = true;
        let mut site_worst = SiteClass::SameOrigin;
        let mut prev: Option<String> = None;
        loop {
            if let Some(initiator) = referer {
                let hop_site = classify_site(initiator, &current);
                site_worst = if first {
                    hop_site
                } else {
                    site_worst.max(hop_site)
                };
            }
            let ref_arg = if first {
                referer
            } else if referer.is_some() {
                prev.as_deref()
            } else {
                None
            };
            let site_arg = if referer.is_some() && !first {
                Some(site_worst.value())
            } else {
                None
            };
            let extra = Self::extra_headers(
                &current,
                &[],
                ref_arg,
                site_arg,
                identity,
                self.profile.user_agent.as_str(),
                self.profile.sec_ch_ua.as_str(),
            );
            let hop_started = Instant::now();
            let out = match self
                .wire_once(
                    &current,
                    kind_for(identity.class),
                    proxy.as_ref(),
                    use_jar,
                    extra,
                    false,
                    identity.legacy_user_agent.is_some(),
                )
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    if let (Some(pool), Some((host, id))) = (&self.egress, &pool_lane) {
                        note_lane_outcome(pool, host, id, &e);
                    }
                    return Err(e);
                }
            };
            if let (Some(pool), Some((host, id))) = (&self.egress, &pool_lane) {
                match out.status {
                    429 => pool.note_fetch_rate_limited(host, id),
                    200..=299 => {
                        pool.report_ok(host, id);
                        pool.observe_rtt(id, hop_started.elapsed());
                    }
                    _ => {}
                }
            }
            match out.status {
                301 | 302 | 303 | 307 | 308 => {
                    redirects += 1;
                    first = false;
                    if redirects > MAX_REDIRECTS {
                        return Err(FetchError::TooManyRedirects);
                    }
                    let Some(loc) = header_value(&out.headers, "location") else {
                        let mut o = out;
                        o.elapsed = started.elapsed();
                        o.redirects = redirects;
                        o.route = route;
                        return Ok(o);
                    };
                    let base = url::Url::parse(&current)
                        .map_err(|_| FetchError::InvalidUrl(current.clone()))?;
                    // Centralized redirect SSRF guard: validates scheme,
                    // credentials and host, and rejects private literals.
                    let next = match crate::fetch::guards::validate_redirect_url(&base, &loc) {
                        Ok(u) => u,
                        Err(e) => {
                            // Non-web scheme: return the redirect honestly
                            // (matched on the variant, never the text).
                            if matches!(e, FetchError::NonHttpRedirect(_)) {
                                let mut o = out;
                                o.elapsed = started.elapsed();
                                o.redirects = redirects;
                                o.route = route;
                                return Ok(o);
                            }
                            return Err(e);
                        }
                    };
                    // Pre-navigation redirect policy (crawl scope and
                    // robots): a refused hop is never dialed; the redirect
                    // response returns as the outcome so the caller records
                    // the honest skip.
                    if !redirect_gate(next.clone()).await {
                        let mut o = out;
                        o.elapsed = started.elapsed();
                        o.redirects = redirects;
                        o.route = route;
                        return Ok(o);
                    }
                    prev = Some(current.clone());
                    current = next.to_string();
                }
                _ => {
                    let mut o = out;
                    o.elapsed = started.elapsed();
                    o.redirects = redirects;
                    o.route = route;
                    return Ok(o);
                }
            }
        }
    }

    // ── public surface (kept verbatim for the 13 importers) ─────────────

    /// Fetch with browser-correct redirects, cookies, cache revalidation.
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

    /// Full lane control: `use_jar=false` keeps the shared cookie
    /// jar OUT of the request.
    pub async fn fetch_via_jar(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
    ) -> Result<FetchOutcome, FetchError> {
        self.fetch_via_jar_ref(url_str, proxy, use_jar, None).await
    }

    /// Evidence-grade cold probe (v4 phase 0.2): no shared cookie
    /// jar (a true cold client) and no state-plane cache (a cached page is
    /// not evidence about the wall RIGHT NOW). One wire read.
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

    /// Full-knobs variant: `skip_cache` bypasses the state plane entirely
    /// (probe path only; everything else keeps it). `pool_pick` gates the
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
        self.fetch_via_jar_language(
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

    /// As `fetch_via_jar_opts`, with the caller's persona language: a
    /// crawl page must present the same language identity as web_fetch
    /// and the ghost for the same host (v4 E2 coherence).
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_via_jar_language(
        &self,
        url_str: &str,
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        skip_cache: bool,
        pool_pick: bool,
        accept_language: Option<&str>,
        redirect_gate: Option<RedirectGate>,
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
                accept_language,
            },
            None,
            redirect_gate,
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
        redirect_gate: Option<RedirectGate>,
    ) -> Result<FetchOutcome, FetchError> {
        // Centralized URL safety gate: scheme, credentials, localhost and
        // private literals. No dial can follow a cached return. The async
        // DNS-aware tier runs exactly once per request, inside the wire
        // call (or the engine's document GETs).
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

        // The app's hop loop owns every gate-carrying or state-plane-bypassing
        // fetch: the async scope/robots gate must run BEFORE a redirect target
        // is dialed (a crawl read is also a live probe) and it cannot ride the
        // engine's sync redirect consult. Document fetches WITHOUT a gate take
        // the engine path, whose hops the consult re-gates (rules/SSRF).
        if redirect_gate.is_some() || skip_cache {
            let gate = redirect_gate.unwrap_or_else(allow_all_gate);
            return self
                .hop_loop(
                    url_str,
                    hop_proxy,
                    use_jar,
                    referer,
                    identity,
                    gate,
                    route,
                    pool_lane_key,
                )
                .await;
        }

        // Documents: the engine's pipeline (cache, JS, learned hosts).
        let extra = Self::extra_headers(
            url_str,
            &[],
            referer,
            None,
            identity,
            self.profile.user_agent.as_str(),
            self.profile.sec_ch_ua.as_str(),
        );
        let mut out = self
            .doc_fetch(url_str, hop_proxy, use_jar, route.clone(), extra)
            .await?;
        if let (Some(pool), Some((host, id))) = (&self.egress, &pool_lane_key) {
            match out.status {
                429 => pool.note_fetch_rate_limited(host, id),
                200..=299 => {
                    pool.report_ok(host, id);
                    pool.observe_rtt(id, out.elapsed);
                }
                _ => {}
            }
        }
        out.route = route;
        Ok(out)
    }

    /// One cookie-less hop with a request-local legacy User-Agent.
    /// The legacy identity strips client hints and fetch metadata so the
    /// swapped UA reads coherent; URL guards still run.
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

    /// Same, optionally through a CONNECT proxy.
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
    /// carry the per-class header set on the wire.
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

    async fn fetch_once_via_identity(
        &self,
        url_str: &str,
        conditional: &[(String, String)],
        proxy: Option<&proxy::Proxy>,
        use_jar: bool,
        referer: Option<&str>,
        identity: RequestIdentity<'_>,
    ) -> Result<FetchOutcome, FetchError> {
        let extra = Self::extra_headers(
            url_str,
            conditional,
            referer,
            None,
            identity,
            self.profile.user_agent.as_str(),
            self.profile.sec_ch_ua.as_str(),
        );
        self.wire_once(
            url_str,
            kind_for(identity.class),
            proxy,
            use_jar,
            extra,
            true,
            identity.legacy_user_agent.is_some(),
        )
        .await
    }

    /// Tier-1 navigation-identity fetch (the daemon's Chrome-class
    /// hop). Follows redirects (bounded, engine-side). Honors the fetch
    /// pool opt-in like every other fetch path (`proxy.fetch_rotate`).
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
            None,
        )
        .await
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

/// The allow-all gate for callers without a scope policy: the hop loop
/// still runs — every hop is validated — nothing is refused by policy.
fn allow_all_gate() -> RedirectGate {
    std::sync::Arc::new(|_url: url::Url| Box::pin(async { true }))
}

/// The class → wire-kind mapping for the engine's per-kind header sets.
fn kind_for(class: RequestClass) -> ReqKind {
    match class {
        RequestClass::Script => ReqKind::Script,
        RequestClass::Style => ReqKind::Style,
        RequestClass::Font => ReqKind::Font,
        RequestClass::Image | RequestClass::Favicon => ReqKind::Image,
        RequestClass::Navigation => ReqKind::Document,
    }
}

/// The per-hop consult behind engine-followed redirect chains: the same
/// sync gate the app's own hop loop runs (scheme / credentials / rules /
/// SSRF literal), BEFORE the engine dials the hop target. A refusal lands
/// in `refusal` as its typed error — the engine carries only the `denied:`
/// marker + URL — and a non-web scheme stops the chain so the 3xx itself
/// returns as the outcome (v4 semantics).
fn hop_consult(refusal: std::sync::Arc<Mutex<Option<FetchError>>>) -> HopConsult {
    HopConsult::new(move |prev, next| {
        let verdict = match url::Url::parse(prev) {
            Ok(base) => crate::fetch::guards::validate_redirect_url(&base, next),
            // The engine hands resolved absolute URLs; an unparseable
            // previous URL means something upstream is broken — refuse.
            Err(_) => return HopDecision::Deny(next.to_string()),
        };
        match verdict {
            Ok(_) => HopDecision::Follow,
            Err(FetchError::NonHttpRedirect(_)) => HopDecision::Stop,
            Err(e) => {
                *refusal.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                HopDecision::Deny(next.to_string())
            }
        }
    })
}

/// The error path shared by the engine-backed fetches: a `denied:` failure
/// returns the typed refusal the consult recorded (the engine carries only
/// the URL); anything else maps by class.
fn prefer_refusal(raw: String, refusal: &Mutex<Option<FetchError>>) -> FetchError {
    if raw.contains("denied:")
        && let Some(e) = refusal.lock().unwrap_or_else(|p| p.into_inner()).take()
    {
        return e;
    }
    map_engine_err(raw)
}

/// Path of the vault-domain ledger: beside the engine jar under the same
/// state root — the ledger and the jar are one state plane and must move
/// together.
fn vault_domains_path() -> Option<std::path::PathBuf> {
    doncore::state::state_root().map(|root| root.join("v1").join("vault-domains.json"))
}

/// The domains the vault fed the jar, across runs. Read fresh (never cached
/// at construction); missing or unreadable reads as empty — the ledger is
/// best-effort and never fails a fetch.
fn load_vault_domains() -> std::collections::HashSet<String> {
    let Some(path) = vault_domains_path() else {
        return Default::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Default::default();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .map(|v| v.into_iter().filter_map(|d| normalize_domain(&d)).collect())
        .unwrap_or_default()
}

/// Persist the ledger atomically (tmp + rename). Best-effort: a failed
/// write leaves the in-memory set authoritative and says so once.
fn save_vault_domains(domains: &std::collections::HashSet<String>) {
    let Some(path) = vault_domains_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let mut list: Vec<&str> = domains.iter().map(String::as_str).collect();
    list.sort_unstable();
    let Ok(json) = serde_json::to_string(&list) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    let write = std::fs::create_dir_all(parent)
        .and_then(|_| std::fs::write(&tmp, json.as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = write {
        crate::diag!("[fetch] vault-domain ledger write failed: {e}");
    }
}

/// The jar's flat domain form: lowercase, no leading dot.
fn normalize_domain(raw: &str) -> Option<String> {
    let s = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    (!s.is_empty()).then_some(s)
}

/// Map a classified engine failure onto the app taxonomy. The engine's
/// class prefixes are pinned by its tests (`dns:`, `dns-timeout:`,
/// `timeout:`, `tls:`, `connect:`, `proxy:`, `body:`, `redirect:`,
/// `request:`, `internal:`, `denied:`); the engine may wrap them in a phase
/// prefix (`wire: …`), stripped first. An unrecognized shape stays an
/// honest `Http` message rather than a guessed class.
fn map_engine_err(raw: String) -> FetchError {
    let msg = raw.strip_prefix("wire: ").unwrap_or(&raw);
    let (class, detail) = msg
        .split_once(':')
        .map_or(("", msg), |(c, d)| (c, d.trim()));
    match class {
        "dns" => FetchError::Dns(detail.to_owned()),
        "dns-timeout" => FetchError::DnsTimeout(detail.to_owned()),
        "timeout" => FetchError::Timeout,
        "tls" => FetchError::Tls(detail.to_owned()),
        "connect" => FetchError::Io(std::io::Error::other(detail.to_owned())),
        "proxy" => {
            // A proxy demanding credentials keeps the old auth-fail shape
            // (lane_note keys on it); anything else is a dead lane. The
            // engine names the 407 (its tunnel text alone carries only
            // "proxy authorization required"); the marker phrase is
            // accepted too so either side reads alone.
            if detail.contains("407") || detail.contains("proxy authorization required") {
                FetchError::Http(format!("CONNECT -> 407 ({detail})"))
            } else {
                FetchError::Io(std::io::Error::other(format!("proxy: {detail}")))
            }
        }
        "denied" => {
            // A hop the engine's redirect consult refused: rebuild the
            // typed refusal (rules/SSRF payload) from the refused URL.
            match crate::fetch::guards::validate_url_basic(detail) {
                Err(e) => e,
                Ok(_) => FetchError::Http(format!("denied: {detail}")),
            }
        }
        "redirect" => FetchError::TooManyRedirects,
        "body" | "request" | "internal" => FetchError::Http(format!("{class}: {detail}")),
        _ => FetchError::Http(raw),
    }
}

/// As `wire_call`, but the job is cooperatively cancelled when the async
/// caller goes away: a task abort or daemon shutdown drops this future,
/// the guard flags the watch channel, and the worker tears down a pending
/// dial or read instead of holding the connection open.
struct CancelOnDrop(tokio::sync::watch::Sender<bool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

async fn wire_call_cancel(
    wire: doncore::wire::Wire,
    mut req: doncore::wire::WireReq,
) -> Result<doncore::wire::WireResp, String> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    req.cancel = Some(rx);
    let join = tokio::task::spawn_blocking(move || wire.fetch(req));
    let guard = CancelOnDrop(tx);
    let out = match join.await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("wire bridge task failed: {e}"),
    };
    drop(guard);
    out
}

/// The engine's wire and document fetches are synchronous by design (its
/// worker thread owns the IO). This async bridge runs the blocking wait on
/// a dedicated blocking thread — never on a runtime worker — so a peer on
/// the same runtime (in-process test servers, loopback mocks) keeps being
/// polled while the fetch is pending.
async fn wire_call<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("wire bridge task failed: {e}"),
    }
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

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
/// refusal is about the URL and a protocol error about the origin:
/// none of these says anything about the lane.
fn lane_note(e: &FetchError) -> Option<LaneNote> {
    let msg = e.to_string();
    match e {
        FetchError::Dns(_) | FetchError::DnsTimeout(_) => None,
        FetchError::InvalidUrl(_)
        | FetchError::Ssrf(_)
        | FetchError::Denied { .. }
        | FetchError::NonHttpRedirect(_)
        | FetchError::TooManyRedirects => None,
        FetchError::Timeout => Some(LaneNote::Timeout),
        // A certificate-verify failure is the certificate the far
        // side presented, not lane health. Egress-flavored TLS
        // failures ("handshake aborted" / "cut short") still bench.
        FetchError::Tls(msg) if crate::error::is_cert_verify_failure(msg) => {
            Some(LaneNote::OriginTls)
        }
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
mod engine_exit_tests {
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

    // In-process loopback mock for the wire-bridge regression: the runtime
    // hosting the mock must keep polling while the fetch is pending.
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

    // v5: the app bridges the engine's synchronous wire onto a blocking
    // thread; the runtime hosting an in-process loopback peer must keep
    // polling while the fetch is pending (an inline call deadlocked every
    // same-runtime mock until the wire's own 25s timeout).
    #[tokio::test]
    async fn stealth_v5_oneshot_wire_call_keeps_the_loopback_peer_polled() {
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

    // v5 parity: the engine's document path decodes bodies as text, so a
    // PDF must ride the raw wire — byte-exact, or DonSheet reads mojibake.
    // Both the URL-shape route (`/paper.pdf`) and the surprise
    // content-type route (`/download`) must return the exact bytes.
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
        // 1) The URL shape routes raw before any document pipeline runs.
        let shaped = fetcher.fetch(&format!("{base}/paper.pdf")).await.unwrap();
        assert_eq!(shaped.status, 200);
        assert_eq!(
            shaped.body, PDF_BODY,
            "a .pdf URL must arrive byte-exact (lossy text decode would alter the bytes)"
        );
        // 2) A URL that does not look like a PDF but answers as one: the
        //    surprise case re-reads the bytes raw.
        let surprise = fetcher.fetch(&format!("{base}/download")).await.unwrap();
        assert_eq!(surprise.status, 200);
        assert_eq!(
            surprise.body, PDF_BODY,
            "a PDF answer on a non-pdf URL must arrive byte-exact"
        );
        server.abort();
    }
}
