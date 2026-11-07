//! One request on the wire. No retries here, no policy here: this module knows
//! how to send a request as a given identity and how to report exactly what came
//! back. Policy lives in `fetch.rs`.

use crate::envelope::Guard;
use crate::identity::Identity;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wreq::header::{HeaderMap, HeaderName, HeaderValue};

#[derive(Clone, Debug, serde::Serialize)]
pub struct SetCookie {
    pub name: String,
    pub value: String,
    pub domain: Option<String>,
    pub path: Option<String>,
    pub expires_unix: Option<i64>,
    pub secure: bool,
    pub http_only: bool,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Response {
    pub status: u16,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub url: String,
    pub elapsed_ms: f64,
    pub cookies: Vec<SetCookie>,
    /// The body's own bytes when they are not UTF-8 text, so a binary payload
    /// reaches a page program or a caller unchanged.
    #[serde(skip)]
    pub raw: Option<Vec<u8>>,
}

/// What a request is to the browser it presents as: a top-level navigation, or a
/// page's own subresource (a script, `fetch`, XHR or beacon). The two carry
/// different fetch metadata and, in Chrome, a different header order; sending a
/// subresource with `sec-fetch-mode: navigate` is a request no browser makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Navigate,
    Subresource,
}

/// One request, fully described.
pub struct Call<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub cookie: Option<&'a str>,
    pub timeout: Duration,
    /// Caller headers. For a subresource these include its fetch metadata
    /// (`sec-fetch-*`, `accept`, `referer`, `origin`, `content-type`).
    pub extra: &'a [(&'a str, &'a str)],
    pub body: Option<&'a [u8]>,
    /// Refuse a response body larger than this many bytes.
    pub cap: Option<usize>,
    pub kind: Kind,
}

/// Chrome's header order for a subresource on HTTP/2, measured from the
/// reference browser (scripts, `fetch` and XHR). Author headers sit after
/// `sec-ch-ua-platform`; `origin` sits before the fetch metadata on a request
/// with a body and after it on a CORS GET.
const CHROME_SUBRESOURCE_HEAD: &[&str] = &[
    "pragma",
    "cache-control",
    "content-length",
    "sec-ch-ua-platform",
];
const CHROME_SUBRESOURCE_TAIL: &[&str] = &[
    "user-agent",
    "sec-ch-ua",
    "content-type",
    "sec-ch-ua-mobile",
    "accept",
    "origin",
    "sec-fetch-site",
    "sec-fetch-mode",
    "sec-fetch-dest",
    "sec-fetch-storage-access",
    "referer",
    "accept-encoding",
    "accept-language",
    "cookie",
    "priority",
];

/// Headers that belong to a top-level navigation only.
const NAVIGATION_ONLY: &[&str] = &["upgrade-insecure-requests", "sec-fetch-user"];

/// Base64 (standard alphabet, padded).
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (a << 16) | (b << 8) | c;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The largest document body read: the product's own cap on a response.
pub const DOCUMENT_BODY_MAX: usize = 64 << 20;

pub struct Wire {
    client: wreq::Client,
    /// Always `Some` until drop, which hands it to `shutdown_background`.
    runtime: Option<tokio::runtime::Runtime>,
    /// Set on the engine's client: responses are read incrementally and refused
    /// past this many bytes, so a hostile server cannot fill memory inside the
    /// timeout. The document path leaves it unset.
    max_body: Option<usize>,
    /// The engine's client: its guard follows redirects inside the client.
    /// Every other wire follows them itself, one hop at a time, under `policy`.
    guarded: bool,
    /// How the requests of the fetch in progress treat redirects and what a
    /// navigation carries besides the identity's own headers.
    policy: std::sync::Mutex<Policy>,
}

/// Redirect hops a request follows before it fails: Chrome's own limit is 20,
/// the product's has always been 10.
pub const REDIRECTS_MAX: usize = 10;

/// A redirect hop's verdict, asked before the hop is requested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hop {
    /// Request the next URL.
    Follow,
    /// Do not request it: the redirect response itself is the answer.
    Stop,
    /// Refuse it: the request fails with `denied: <reason>`.
    Deny(String),
}

/// Consulted with (the URL that redirected, the absolute URL it names) before
/// every hop is dialed. It runs outside the wire's runtime, so it may block.
pub type HopGuard = Arc<dyn Fn(&str, &str) -> Hop + Send + Sync>;

/// The caller's rules for the requests of one fetch.
#[derive(Clone)]
pub struct Policy {
    /// Follow redirects (each hop asked of `hop` first); off returns the 3xx.
    pub follow: bool,
    pub hop: Option<HopGuard>,
    /// Headers every navigation of this fetch carries over the identity's own
    /// (a link's `referer` and `sec-fetch-site`). Never sent on a subresource.
    pub navigation: Vec<(String, String)>,
    /// The longest one request may take, whatever its caller's budget: inside
    /// an encounter a stalled identity must leave time for the next one.
    pub request_max: Duration,
    /// Stops the fetch when flipped.
    pub cancel: Option<Cancel>,
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Policy")
            .field("follow", &self.follow)
            .field("hop", &self.hop.as_ref().map(|_| "guard"))
            .field("navigation", &self.navigation)
            .field("request_max", &self.request_max)
            .field("cancel", &self.cancel)
            .finish()
    }
}

/// A caller's way to stop a fetch it no longer wants: the request in flight
/// is dropped (its connection with it) and no further request starts. Clones
/// share one switch.
#[derive(Clone, Default)]
pub struct Cancel(Arc<CancelSwitch>);

#[derive(Default)]
struct CancelSwitch {
    flipped: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0
            .flipped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.wake.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flipped.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        loop {
            let woken = self.0.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            woken.await;
        }
    }
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cancel({})", self.is_cancelled())
    }
}

/// The error a cancelled request ends with.
pub const CANCELLED: &str = "cancelled: the caller stopped waiting";

/// One request's ceiling inside an encounter.
pub const REQUEST_MAX: Duration = Duration::from_secs(8);

impl Default for Policy {
    fn default() -> Self {
        Self {
            follow: true,
            hop: None,
            navigation: Vec::new(),
            request_max: REQUEST_MAX,
            cancel: None,
        }
    }
}

/// The verdict on a hop no caller rule covers: the web's own schemes are
/// followed, anything else (`mailto:`, an app link) ends the chain on the 3xx.
pub fn default_hop(next: &str) -> Hop {
    match url::Url::parse(next) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Hop::Follow,
        _ => Hop::Stop,
    }
}

/// The `Cookie` header of a redirect hop: the request's own, with what the
/// earlier hops of this host set applied in order (a later value replaces an
/// earlier one; an expired one removes it), as far as their paths cover `url`.
fn hop_cookie_header(original: Option<&str>, set: &[SetCookie], url: &str) -> Option<String> {
    let path = url::Url::parse(url)
        .map(|url| url.path().to_string())
        .unwrap_or_else(|_| "/".to_string());
    let mut jar: Vec<(String, String)> = original
        .unwrap_or_default()
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            Some((name.to_string(), value.to_string()))
        })
        .collect();
    let now = unix_now();
    for cookie in set {
        if cookie
            .path
            .as_deref()
            .is_some_and(|scope| !path.starts_with(scope))
        {
            continue;
        }
        jar.retain(|(name, _)| name != &cookie.name);
        if cookie.expires_unix.is_none_or(|expires| expires > now) {
            jar.push((cookie.name.clone(), cookie.value.clone()));
        }
    }
    (!jar.is_empty()).then(|| {
        jar.iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

/// Headers that validate one URL's cached copy; a redirect hop never carries them.
const CONDITIONALS: &[&str] = &["if-none-match", "if-modified-since"];

fn host_and_scheme(url: &str) -> Option<(String, String)> {
    let url = url::Url::parse(url).ok()?;
    Some((
        url.host_str()?.to_ascii_lowercase(),
        url.scheme().to_string(),
    ))
}

/// Whether an error's words carry the status 407 itself, not those digits
/// inside a port or a longer number (`127.0.0.1:40789`).
fn names_407(lower: &str) -> bool {
    lower.match_indices("407").any(|(at, _)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + 3..].chars().next();
        !before.is_some_and(|c| c.is_ascii_digit() || c == ':' || c == '.')
            && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// A transport failure as one line whose prefix names its class (`timeout:`,
/// `dns:`, `tls:`, `proxy:`, `connect:`, `body:`, `request:`), so a caller maps
/// it without guessing from prose.
fn describe(error: &wreq::Error) -> String {
    let mut chain = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    let lower = chain.to_ascii_lowercase();
    let class = if error.is_timeout() || lower.contains("timed out") {
        "timeout"
    } else if error.is_proxy_connect()
        || lower.contains("proxy")
        || names_407(&lower)
        || lower.contains("socks")
    {
        "proxy"
    } else if error.is_dns()
        || lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("resolve")
    {
        "dns"
    } else if error.is_tls() || lower.contains("certificate") || lower.contains("handshake") {
        "tls"
    } else if error.is_connect() || error.is_connection_reset() {
        "connect"
    } else if error.is_body() || error.is_decode() {
        "body"
    } else {
        "request"
    };
    // The tunnel error names a refused credential only in words; the status is
    // what a caller benches a lane on.
    if class == "proxy" && lower.contains("proxy authorization required") && !names_407(&lower) {
        return format!("proxy: 407 {chain}");
    }
    format!("{class}: {chain}")
}

// A runtime dropped inside an async context panics (tokio refuses to block
// there), and a caller may drop its session anywhere: shut down without waiting.
impl Drop for Wire {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Chrome's document-request header order, measured from the reference browser's
/// own request (`sec-fetch-site` before `mode`, hints before the UA, and so on).
/// The profile ships a different order; the set was the same, the order was not.
const CHROME_DOCUMENT_ORDER: &[&str] = &[
    "sec-ch-ua",
    "sec-ch-ua-mobile",
    "sec-ch-ua-platform",
    "upgrade-insecure-requests",
    "user-agent",
    "accept",
    "sec-fetch-site",
    "sec-fetch-mode",
    "sec-fetch-user",
    "sec-fetch-dest",
    "accept-encoding",
    "accept-language",
    "priority",
];

/// The identity's own TLS options, so changing one flag keeps the rest.
fn builder_options(identity: &Identity) -> wreq::tls::TlsOptions {
    identity
        .emulation()
        .and_then(|emulation| emulation.tls_options)
        .unwrap_or_default()
}

/// Extra trust roots, PEM, a caller adds before the first request: an owner's
/// own configured CA bundle (the product's `[tls] cert_file`/`cert_dir`).
static EXTRA_ROOTS: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

/// Trust the PEM certificates in `pem` too, on top of the bundled and OS
/// roots. Takes effect for clients built after the first call that builds the
/// store, so it belongs before the first request.
pub fn add_trust_pem(pem: Vec<u8>) {
    EXTRA_ROOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(pem);
}

/// The roots a server's chain is verified against: the bundled Mozilla set,
/// which is the same everywhere, plus what the OS itself trusts (Windows' ROOT
/// store, the macOS keychain, the Linux bundle, `SSL_CERT_FILE`/`SSL_CERT_DIR`),
/// as Chrome trusts the OS store: an enterprise's inspection root or a private
/// CA installed the OS's way is trusted here too, and so is what the owner added
/// with [`add_trust_pem`]. Read once per process. A bundle that does not parse
/// is left out, never the whole store; `None` (the store could not be built)
/// leaves BoringSSL's default paths.
fn trust_roots() -> Option<wreq::tls::trust::CertStore> {
    static ROOTS: std::sync::LazyLock<Option<wreq::tls::trust::CertStore>> =
        std::sync::LazyLock::new(|| {
            let native = rustls_native_certs::load_native_certs().certs;
            let base = || {
                wreq::tls::trust::CertStore::builder()
                    .add_der_certs(
                        webpki_root_certs::TLS_SERVER_ROOT_CERTS
                            .iter()
                            .map(|cert| cert.as_ref()),
                    )
                    .add_der_certs(native.iter().map(|cert| cert.as_ref()))
            };
            let extra = EXTRA_ROOTS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let mut builder = base();
            for pem in &extra {
                // Each bundle alone first: one that does not parse is dropped
                // and named, and the rest of the store stands.
                if base().add_stack_pem_certs(pem).build().is_ok() {
                    builder = builder.add_stack_pem_certs(pem);
                } else {
                    eprintln!("[trust] an added CA bundle is not PEM certificates; left out");
                }
            }
            builder.build().ok().or_else(|| base().build().ok())
        });
    ROOTS.clone()
}

/// The proxy schemes the transport dials.
const PROXY_SCHEMES: &[&str] = &["http", "https", "socks4", "socks4a", "socks5", "socks5h"];

/// `raw` read as a proxy URL and written back out as the URL a client is
/// handed for it (scheme in lowercase, user name and password
/// percent-encoded), so the endpoint dialed is the endpoint read. Readable
/// means `scheme://[user[:password]@]host[:port][/]` with a proxy scheme
/// (`http`, `https`, `socks4`, `socks4a`, `socks5`, `socks5h`, in any case).
/// Anything else is a one-line error that names the proxy with its user name
/// and password masked.
pub fn proxy_url(raw: &str) -> Result<String, String> {
    let refuse = |why: &str| {
        // A '/', '?' or '#' inside the credentials ends the address early,
        // and the reason a parser gives for that ("invalid port number")
        // does not point at the credentials.
        let hint = if userinfo(raw).is_some_and(|span| raw[span].contains(['/', '?', '#'])) {
            "; a '/', '?' or '#' in the user name or password must be percent-encoded"
        } else {
            ""
        };
        format!("cannot read proxy URL {:?}: {why}{hint}", masked(raw))
    };
    let url = url::Url::parse(raw).map_err(|error| refuse(&error.to_string()))?;
    if !PROXY_SCHEMES.contains(&url.scheme()) {
        // The scheme is not echoed: in "user:pass@host:port" it is the user name.
        return Err(refuse(&format!(
            "not a proxy scheme ({})",
            PROXY_SCHEMES.join(", ")
        )));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(refuse("no host"));
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(refuse("a path, query or fragment after host:port"));
    }
    Ok(url.into())
}

/// Where a user name and password sit in `raw`, however the rest of the URL
/// breaks: from after a leading `scheme://` to the last `@`.
fn userinfo(raw: &str) -> Option<std::ops::Range<usize>> {
    let at = raw.rfind('@')?;
    let start = raw[..at]
        .find("://")
        .filter(|&end| {
            let scheme = &raw[..end];
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        })
        .map_or(0, |end| end + 3);
    Some(start..at)
}

/// `raw` with its user name and password replaced by `***`.
fn masked(raw: &str) -> String {
    match userinfo(raw) {
        Some(span) => format!("{}***{}", &raw[..span.start], &raw[span.end..]),
        None => raw.to_string(),
    }
}

impl Wire {
    /// A client that presents `identity` coherently, through an egress proxy (`http://`, `https://`,
    /// `socks5://` URL, credentials in the userinfo), or direct when `None`: the proxy variables of
    /// the environment are not read.
    pub fn via(identity: &Identity, proxy: Option<&str>) -> Result<Self, String> {
        Self::build(identity, None, proxy)
    }

    /// The same client for a page program's own requests: every redirect hop and
    /// every resolved address is validated against `guard`, and bodies are capped.
    /// It leaves through the same egress as the document, since a vendor binds
    /// what it issues to the address that asked.
    pub fn guarded(
        identity: &Identity,
        guard: &Arc<Guard>,
        proxy: Option<&str>,
    ) -> Result<Self, String> {
        Self::build(identity, Some(guard), proxy)
    }

    fn build(
        identity: &Identity,
        guard: Option<&Arc<Guard>>,
        proxy: Option<&str>,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("runtime: {error}"))?;
        let mut builder = wreq::Client::builder()
            // Hops are followed by `Wire::call`, one request each, so every hop
            // is asked of the caller's guard before it is dialed and a cached
            // page's validators never ride one. The engine's client overrides
            // this with its guard's policy below.
            .redirect(wreq::redirect::Policy::none())
            // No hidden retries: a 429 is information the fetcher acts on, not
            // something a library should hide behind a backoff.
            .retry(wreq::retry::Policy::never())
            .connect_timeout(Duration::from_millis(2500))
            .timeout(Duration::from_secs(8))
            // One identity, one pool: connections are reused inside a process.
            .pool_max_idle_per_host(4);
        if let Some(roots) = trust_roots() {
            builder = builder.tls_cert_store(roots);
        }
        if let Some(guard) = guard {
            builder = builder
                .redirect(guard.redirect_policy())
                .dns_resolver(guard.resolver());
        }
        if let Some(proxy) = proxy {
            // Through a proxy the name is resolved on the proxy's side: the
            // guard still refuses literal private addresses before any socket.
            // wreq accepts a proxy URL its matcher cannot read, keeps no proxy
            // for it and dials the target direct, so it is only ever handed
            // the URL `proxy_url` read. Its own error is stripped of the URL,
            // which carries the credentials.
            let url = proxy_url(proxy).map_err(|error| format!("proxy: {error}"))?;
            let proxy =
                wreq::Proxy::all(url).map_err(|error| format!("proxy: {}", error.without_uri()))?;
            builder = builder.proxy(proxy);
        } else {
            // wreq's builder otherwise reads HTTP_PROXY, HTTPS_PROXY and
            // ALL_PROXY itself (auto_sys_proxy, on unless proxy() or
            // no_proxy() is called). The caller chose the route: with none
            // handed over, the client is direct.
            builder = builder.no_proxy();
        }
        if let Some(mut emulation) = identity.emulation() {
            if identity.name.starts_with("chrome") {
                let mut ordered = wreq::header::OrigHeaderMap::new();
                for name in CHROME_DOCUMENT_ORDER {
                    ordered.insert(*name);
                }
                // Anything the profile carries beyond that list keeps its place,
                // appended in its own order.
                ordered.extend(std::mem::take(&mut emulation.orig_headers));
                emulation.orig_headers = ordered;
            }
            builder = builder.emulation(emulation);
            // Chrome (and only Chrome) permutes ClientHello extension order on
            // every connection: BoringSSL's permute_extensions. Without it our
            // JA3 is one fixed hash — a stable fingerprint no real Chrome has.
            // Firefox and Safari send a fixed order, so permuting them would be
            // its own lie.
            if identity.name.starts_with("chrome") {
                let mut options = builder_options(identity);
                options.permute_extensions = Some(true);
                builder = builder.tls_options(options);
            }
        }
        let client = builder
            .build()
            .map_err(|error| format!("client: {error}"))?;
        Ok(Self {
            client,
            runtime: Some(runtime),
            max_body: guard.map(|guard| guard.limits.response_body_max),
            guarded: guard.is_some(),
            policy: std::sync::Mutex::new(Policy::default()),
        })
    }

    /// The rules the next requests on this wire follow (one fetch's).
    pub fn set_policy(&self, policy: Policy) {
        *self
            .policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
    }

    pub fn get(
        &self,
        url: &str,
        cookie_header: Option<&str>,
        identity: &Identity,
        timeout: Duration,
    ) -> Result<Response, String> {
        self.request("GET", url, cookie_header, identity, timeout, &[], None)
    }

    /// A page request that carries the arrival headers a browser would send when
    /// it navigates within a site it has already been on.
    pub fn get_arriving(
        &self,
        url: &str,
        cookie_header: Option<&str>,
        identity: &Identity,
        timeout: Duration,
        referer: &str,
    ) -> Result<Response, String> {
        let extra = [
            ("referer", referer),
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "document"),
        ];
        self.request("GET", url, cookie_header, identity, timeout, &extra, None)
    }

    /// One request with the identity's headers plus the caller's own. Used for
    /// requests that are not a page load: an app's XHR, a GQL post.
    #[allow(clippy::too_many_arguments)]
    pub fn request(
        &self,
        method: &str,
        url: &str,
        cookie_header: Option<&str>,
        identity: &Identity,
        timeout: Duration,
        extra: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<Response, String> {
        self.request_capped(
            method,
            url,
            cookie_header,
            identity,
            timeout,
            extra,
            body,
            self.max_body,
        )
    }

    /// `request`, refusing a response body larger than `cap` bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn request_capped(
        &self,
        method: &str,
        url: &str,
        cookie_header: Option<&str>,
        identity: &Identity,
        timeout: Duration,
        extra: &[(&str, &str)],
        body: Option<&str>,
        cap: Option<usize>,
    ) -> Result<Response, String> {
        self.call(
            identity,
            Call {
                method,
                url,
                cookie: cookie_header,
                timeout,
                extra,
                body: body.map(str::as_bytes),
                cap,
                kind: Kind::Navigate,
            },
        )
    }

    /// A page's own request (script, `fetch`, XHR, beacon, or an app's API call):
    /// the identity's client hints and UA, the caller's fetch metadata, and, for a
    /// Chrome identity, Chrome's own subresource header order. No navigation-only
    /// header is sent.
    pub fn subresource(&self, identity: &Identity, call: Call<'_>) -> Result<Response, String> {
        self.call(
            identity,
            Call {
                kind: Kind::Subresource,
                ..call
            },
        )
    }

    /// One request exactly as described, its redirects followed hop by hop under
    /// the wire's policy: each hop asked of the hop guard before it is dialed,
    /// a cached page's validators sent on the first hop only, the cookie sent
    /// only to the host (and scheme) it was read for, and only that host's
    /// `Set-Cookie`s kept. The response is the last hop's, `url` its URL.
    pub fn call(&self, identity: &Identity, call: Call<'_>) -> Result<Response, String> {
        let policy = self
            .policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let call = Call {
            timeout: call.timeout.min(policy.request_max),
            ..call
        };
        if self.guarded {
            return self.send(identity, call);
        }
        let started = Instant::now();
        let first = host_and_scheme(call.url);
        let mut extra: Vec<(&str, &str)> = call.extra.to_vec();
        if call.kind == Kind::Navigate {
            for (name, value) in &policy.navigation {
                if !extra
                    .iter()
                    .any(|(known, _)| known.eq_ignore_ascii_case(name))
                {
                    extra.push((name.as_str(), value.as_str()));
                }
            }
        }
        let mut url = call.url.to_string();
        let mut method = call.method.to_string();
        let mut body = call.body;
        let mut kept: Vec<SetCookie> = Vec::new();
        for hop in 0..=REDIRECTS_MAX {
            let here = host_and_scheme(&url);
            let same = here.is_some() && here == first;
            // A browser's next hop carries what the redirect just set (a gate
            // that validates on the first hop opens its session there).
            let hop_cookie = if same && hop > 0 {
                hop_cookie_header(call.cookie, &kept, &url)
            } else if same {
                call.cookie.map(str::to_string)
            } else {
                None
            };
            let headers: Vec<(&str, &str)> = if hop == 0 {
                extra.clone()
            } else {
                extra
                    .iter()
                    .copied()
                    .filter(|(name, _)| {
                        !CONDITIONALS
                            .iter()
                            .any(|conditional| name.eq_ignore_ascii_case(conditional))
                    })
                    .collect()
            };
            let mut response = self.send(
                identity,
                Call {
                    method: &method,
                    url: &url,
                    cookie: hop_cookie.as_deref(),
                    timeout: call.timeout.saturating_sub(started.elapsed()),
                    extra: &headers,
                    body,
                    cap: call.cap,
                    kind: call.kind,
                },
            )?;
            if same {
                kept.append(&mut response.cookies);
            }
            let location = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value.clone());
            let next = match (
                matches!(response.status, 301 | 302 | 303 | 307 | 308) && policy.follow,
                location,
            ) {
                (true, Some(location)) => url::Url::parse(&url)
                    .and_then(|base| base.join(&location))
                    .ok()
                    .map(|next| next.to_string()),
                _ => None,
            };
            let Some(next) = next else {
                response.cookies = kept;
                response.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                return Ok(response);
            };
            let verdict = match &policy.hop {
                Some(guard) => match default_hop(&next) {
                    Hop::Follow => guard(&url, &next),
                    other => other,
                },
                None => default_hop(&next),
            };
            match verdict {
                Hop::Follow => {}
                Hop::Stop => {
                    response.cookies = kept;
                    response.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                    return Ok(response);
                }
                Hop::Deny(reason) => return Err(format!("denied: {reason}")),
            }
            // A 303, and a 301/302 answering a POST, turn the next hop into a GET
            // without a body, as every browser does; 307/308 keep both.
            if response.status == 303
                || (matches!(response.status, 301 | 302) && method.eq_ignore_ascii_case("POST"))
            {
                method = "GET".to_string();
                body = None;
            }
            url = next;
        }
        Err(format!("redirect: more than {REDIRECTS_MAX} redirects"))
    }

    /// One request on the wire, no redirect followed by us.
    fn send(&self, identity: &Identity, call: Call<'_>) -> Result<Response, String> {
        let Call {
            method,
            url,
            cookie: cookie_header,
            timeout,
            extra,
            body,
            cap,
            kind,
        } = call;
        let started = Instant::now();
        let mut headers = HeaderMap::new();
        let put = |headers: &mut HeaderMap, name: &str, value: &str| {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        };
        let emulation = identity.emulation();
        // A Chrome subresource is written out whole, in Chrome's order: the
        // client's defaults are a navigation's header set.
        let explicit =
            kind == Kind::Subresource && identity.name.starts_with("chrome") && emulation.is_some();
        let mut order: Option<wreq::header::OrigHeaderMap> = None;
        if explicit {
            let defaults = &emulation.as_ref().expect("checked").headers;
            for name in [
                "user-agent",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "accept-language",
            ] {
                if let Some(value) = defaults.get(name).and_then(|value| value.to_str().ok()) {
                    put(&mut headers, name, value);
                }
            }
            put(&mut headers, "accept-encoding", "gzip, deflate, br, zstd");
            put(&mut headers, "accept", "*/*");
            put(&mut headers, "priority", "u=1, i");
            for (name, value) in extra {
                put(&mut headers, name, value);
            }
            // Chrome adds Origin on a CORS GET after the fetch metadata, and on
            // any request with a body before it.
            let late_origin =
                method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
            let known: Vec<&str> = CHROME_SUBRESOURCE_HEAD
                .iter()
                .chain(CHROME_SUBRESOURCE_TAIL.iter())
                .copied()
                .collect();
            let mut ordered = wreq::header::OrigHeaderMap::new();
            for name in CHROME_SUBRESOURCE_HEAD {
                ordered.insert(*name);
            }
            // Author headers (anything Chrome does not add itself) come next.
            for (name, _) in extra {
                let lower = name.to_ascii_lowercase();
                if !known.contains(&lower.as_str()) && !lower.starts_with("sec-fetch-") {
                    ordered.insert(lower);
                }
            }
            for name in CHROME_SUBRESOURCE_TAIL {
                if *name == "origin" && late_origin {
                    continue;
                }
                ordered.insert(*name);
                if *name == "sec-fetch-dest" && late_origin {
                    ordered.insert("origin");
                }
            }
            order = Some(ordered);
        } else {
            for (name, value) in identity.headers() {
                if kind == Kind::Subresource && NAVIGATION_ONLY.contains(&name) {
                    continue;
                }
                put(&mut headers, name, value);
            }
            for (name, value) in extra {
                put(&mut headers, name, value);
            }
        }
        if let Some(cookies) = cookie_header {
            put(&mut headers, "cookie", cookies);
        }
        let mut request = self
            .client
            .request(
                wreq::Method::from_bytes(method.as_bytes())
                    .map_err(|error| format!("request: {error}"))?,
                url,
            )
            .headers(headers)
            .timeout(timeout);
        if let Some(order) = order {
            request = request.default_headers(false).orig_headers(order);
        }
        if let Some(body) = body {
            request = request.body(body.to_vec()).header(
                HeaderName::from_bytes(b"content-length").unwrap(),
                HeaderValue::from_str(&body.len().to_string()).unwrap(),
            );
        }
        let runtime = self
            .runtime
            .as_ref()
            .expect("a wire's runtime lives until drop");
        let cancel = self
            .policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel
            .clone();
        if cancel.as_ref().is_some_and(Cancel::is_cancelled) {
            return Err(CANCELLED.to_string());
        }
        let exchange = async move {
            let response = request.send().await.map_err(|error| describe(&error))?;
            let status = response.status().as_u16();
            let version = format!("{:?}", response.version());
            let url = response.uri().to_string();
            let cookies: Vec<SetCookie> = response
                .cookies()
                .map(|cookie| SetCookie {
                    name: cookie.name().to_string(),
                    value: cookie.value().to_string(),
                    domain: cookie.domain().map(str::to_string),
                    path: cookie.path().map(str::to_string),
                    // Max-Age wins over Expires (RFC 6265 §5.3 step 3); zero or
                    // less is a deletion, an expiry already past.
                    expires_unix: match cookie.max_age() {
                        Some(age) => Some(unix_now().saturating_add(age.as_secs() as i64)),
                        None => cookie.expires().map(|time| {
                            time.duration_since(std::time::UNIX_EPOCH)
                                .map(|delta| delta.as_secs() as i64)
                                .unwrap_or(0)
                        }),
                    },
                    secure: cookie.secure(),
                    http_only: cookie.http_only(),
                })
                .collect();
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_string(), value.to_string()))
                })
                .collect();
            // Every body is read incrementally against a cap, so a server cannot
            // fill memory inside the timeout; the document path's cap is the
            // product's (64 MiB), the engine's is its envelope's.
            let cap = cap.unwrap_or(DOCUMENT_BODY_MAX);
            if response
                .content_length()
                .is_some_and(|len| len > cap as u64)
            {
                return Err(format!("body: response larger than {cap} bytes"));
            }
            let mut stream = response.bytes_stream();
            let mut collected: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| describe(&error))?;
                if collected.len() + chunk.len() > cap {
                    return Err(format!("body: response larger than {cap} bytes"));
                }
                collected.extend_from_slice(&chunk);
            }
            // Text stays text; anything else keeps its own bytes beside a lossy
            // reading (classification reads text, callers may want the bytes).
            let (body, raw) = match String::from_utf8(collected) {
                Ok(text) => (text, None),
                Err(error) => {
                    let bytes = error.into_bytes();
                    (String::from_utf8_lossy(&bytes).into_owned(), Some(bytes))
                }
            };
            Ok::<_, String>((status, version, url, headers, body, cookies, raw))
        };
        // wreq starts the request's timeout again for the body, so a slow body
        // could run to twice the budget; the whole exchange is bounded here.
        let exchange = async move {
            tokio::time::timeout(timeout, exchange)
                .await
                .unwrap_or_else(|_| Err("timeout: operation timed out".to_string()))
        };
        let outcome = runtime.block_on(async move {
            match cancel {
                Some(cancel) => tokio::select! {
                    outcome = exchange => outcome,
                    () = cancel.cancelled() => Err(CANCELLED.to_string()),
                },
                None => exchange.await,
            }
        });
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        match outcome {
            Ok((status, version, url, headers, body, cookies, raw)) => Ok(Response {
                status,
                version,
                headers,
                body,
                url,
                elapsed_ms,
                cookies,
                raw,
            }),
            Err(error) => Err(error),
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod describe_tests {
    use super::names_407;

    #[test]
    fn a_407_is_a_status_and_not_a_port() {
        assert!(names_407("proxy: 407 proxy authentication required"));
        assert!(names_407("unsuccessful tunnel (407)"));
        assert!(!names_407(
            "error sending request for uri (http://127.0.0.1:40789/)"
        ));
        assert!(!names_407("http://127.0.0.1:4070/"));
        assert!(!names_407("body of 14070 bytes"));
        assert!(!names_407("version 1.407"));
    }
}
