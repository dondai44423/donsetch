use std::fmt;

#[derive(Debug)]
pub enum FetchError {
    InvalidUrl(String),
    Tls(String),
    Io(std::io::Error),
    Http(String),
    /// Invalid local proxy settings, rather than an origin transport failure.
    ProxyConfig(String),
    /// A local-browser failure (launch, CDP link, capture): the
    /// browser module and its callers produce these.
    Browser(String),
    Timeout,
    /// The response was still arriving when the time ran out: the server
    /// answered and kept sending, the transfer was slower than the budget.
    /// Carries the bytes received.
    SlowTransfer(usize),
    TooManyRedirects,
    /// The host could not be resolved at all: NXDOMAIN, no addresses,
    /// a resolver error. Deliberately its own variant rather than an
    /// `Http` string: this is a NAME problem, and an agent that branches
    /// on `guard.ssrf` would read it as "forbidden by policy" (#248).
    Dns(String),
    /// The resolver did not answer in time. Transient: the same name can
    /// resolve a second later, unlike a name that does not exist.
    DnsTimeout(String),
    /// Resolved to a private/loopback/metadata address, or a URL the
    /// guard refuses on policy. Blocked by design.
    Ssrf(String),
    /// A local DonSeTch rule (`[rules.url."<pattern>"]`) refused this
    /// URL before any request. Self-contained: `rule` is the pattern
    /// key, `message` the operator's guidance verbatim, `kind` is
    /// `"walled"` or `"permanent"`, and `reason` the optional subcode
    /// of the `policy.denied.<reason>` code, and `url` the URL the rule
    /// refused: a redirect hop's target when the refusal came mid-chain.
    /// `Display` prints only a fixed text, never `rule`, `message` or
    /// `url`.
    Denied {
        rule: String,
        message: String,
        kind: &'static str,
        reason: Option<String>,
        url: String,
    },
    /// A redirect `Location` with a scheme other than http(s); carries
    /// that scheme. The redirect is not followed.
    NonHttpRedirect(String),
}

impl FetchError {
    pub fn browser(msg: impl Into<String>) -> Self {
        Self::Browser(msg.into())
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(u) => write!(f, "invalid url: {u}"),
            Self::Tls(e) => write!(f, "tls: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Http(e) => write!(f, "http: {e}"),
            Self::ProxyConfig(e) => write!(f, "proxy configuration: {e}"),
            Self::Browser(e) => write!(f, "browser: {e}"),
            Self::Timeout => write!(f, "timeout"),
            Self::SlowTransfer(received) => {
                write!(f, "timeout: slow transfer, {received} bytes received")
            }
            Self::TooManyRedirects => write!(f, "too many redirects"),
            // The DNS prose carries no "SSRF guard" tail: that tail is
            // what made a typo look like a policy decision.
            Self::Dns(e) => write!(f, "dns: {e}"),
            Self::DnsTimeout(e) => write!(f, "dns timeout: {e}"),
            Self::Ssrf(e) => write!(f, "blocked: {e}"),
            // Fixed text only: neither the rule key nor the operator's
            // message. A stringified error feeds text heuristics
            // (`error_code`'s classifier, the egress-lane `lane_note`),
            // and both strings are operator-chosen: a message mentioning
            // "captcha" or "timeout", or a key such as `dns.lookup.example` or
            // `connect.widgets.example`, would be misread as a wall, a
            // network failure or a dead proxy lane. The surfaces that
            // need the key and the message read the variant's fields.
            // Do not add them here.
            Self::Denied { .. } => write!(f, "excluded by the operator's local DonSeTch rules"),
            Self::NonHttpRedirect(scheme) => {
                write!(f, "blocked redirect to non-http(s) scheme: {scheme}")
            }
        }
    }
}

impl std::error::Error for FetchError {}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ── TLS failure vocabulary ─────────────────────────────────────────────
// Moved from the old transport's tls module (v5 swap, 2026-10-23): the
// wire is the engine now, but the app's TLS classes still key off this
// text — `is_cert_verify_failure` is the one predicate the mcp error
// mapping (tls.verify) and the fetch egress accounting both use, so the
// two can never drift apart.

/// Turn a handshake failure into a short, actionable message. The
/// raw Debug dump of a mid-handshake stream terrifies users and
/// hides the actual cause behind struct fields; the error's Display
/// stack is short and greppable, so classification runs on
/// that plus the io error chain.
pub fn classify_handshake_error<E: std::error::Error>(e: &E) -> String {
    const MANY_IO: [&str; 6] = [
        "ConnectionReset",
        "connection reset by peer",
        "Connection reset",
        "ConnectionRefused",
        "connection refused",
        "ECONNRESET",
    ];
    const MANY_EOF: [&str; 5] = [
        "stream closed",
        "UnexpectedEof",
        "unexpected EOF",
        "SYSCALL",
        "ECONNRESET",
    ];
    let text = e.to_string();
    let low = text.to_lowercase();

    if MANY_IO.iter().any(|m| text.contains(m)) || low.contains("os error 104") {
        return format!(
            "TLS handshake aborted (connection reset or cut mid-negotiation). {}",
            EGRESS_HINT
        );
    }
    if MANY_EOF.iter().any(|m| text.contains(m)) || low.contains("os error 54") {
        return format!(
            "TLS handshake cut short (peer closed the connection). {}",
            EGRESS_HINT
        );
    }
    if low.contains("certificate verify failed")
        || low.contains("verification failed")
        || low.contains("hostname mismatch")
        || low.contains("unknown ca")
        || low.contains("unknown issuer")
        || low.contains("unable to get local issuer")
        || low.contains("self-signed")
        || low.contains("invalid certificate verification")
    {
        return format!("TLS certificate verification failed. {}", CERT_HINT);
    }
    if low.contains("certificate_unknown") || low.contains("tlsv1") || low.contains("no protocols")
    {
        return "TLS handshake failed (protocol/cipher negotiation rejected by the peer)".into();
    }
    text
}

/// True when a classified TLS message describes a certificate/trust
/// failure: the certificate the far side presented, never lane health.
/// Covers the app's own vocabulary (`classify_handshake_error` outputs
/// and `BLOCKED_CA_FAILURE`) and the engine's wording for verify
/// problems (`btls`/`wreq` report the certificate by name).
pub fn is_cert_verify_failure(msg: &str) -> bool {
    let low = msg.to_lowercase();
    msg.starts_with("TLS certificate verification failed")
        || msg.contains("trusted root")
        || low.contains("certificate")
        || low.contains("unknown issuer")
        || low.contains("self-signed")
}

/// The handshake failure for a chain refused by the compromised-CA
/// blocklist. It is a certificate-verify failure
/// (`is_cert_verify_failure` holds), so it keeps that class everywhere.
pub const BLOCKED_CA_FAILURE: &str = "TLS certificate verification failed: the server's chain contains a certificate authority key that is publicly known to be compromised, so DonSeTch refuses it whatever the trust store says. Adding certificates or setting SSL_CERT_FILE does not change this.";

/// True when a classified TLS message is `BLOCKED_CA_FAILURE`.
pub fn is_blocked_ca_failure(msg: &str) -> bool {
    msg.starts_with(BLOCKED_CA_FAILURE)
}

/// What to tell a user when the transport dies mid-handshake: the
/// two situations that produce this are an egress filter killing
/// direct HTTPS (use the env proxy convention, opt out with the
/// kill switch) or an interception proxy that dislikes the exotic
/// Chrome ClientHello (handled automatically when proxied).
const EGRESS_HINT: &str = "The egress path is killing HTTPS out of band: if this machine or sandbox routes egress through an HTTP(S) proxy, export HTTPS_PROXY/HTTP_PROXY (donsetch honors them, NO_PROXY accepted, DONSETCH_NO_ENV_PROXY=1 disables) and point SSL_CERT_FILE at the interception CA bundle. Run `donsetch doctor` for a live egress diagnosis.";

/// Trust-store failure: the interception proxy re-signs every cert,
/// which verifies only against the bundle the network operator gave
/// out (SSL_CERT_FILE). Direct connections to real hosts fail when
/// the CA that signed them isn't in the platform store either.
const CERT_HINT: &str = "The presentation cert chain was not issued by any trusted root. In a TLS-intercepting network the proxy re-signs certificates with its own CA: export SSL_CERT_FILE (or SSL_CERT_DIR) pointing at that bundle so donsetch trusts it, exactly like curl/openssl do. `donsetch doctor` reports both stores.";

#[cfg(test)]
mod tests {
    use super::FetchError;

    #[test]
    fn a_denial_displays_fixed_text_without_operator_strings() {
        let e = FetchError::Denied {
            rule: "connect.widgets.example".into(),
            message: "solve the captcha by hand; a timeout here is expected".into(),
            kind: "walled",
            reason: Some("ip_ban".into()),
            url: "https://connect.widgets.example/captcha".into(),
        };
        let shown = e.to_string();
        assert_eq!(shown, "excluded by the operator's local DonSeTch rules");
        assert!(!shown.contains("facebook") && !shown.contains("captcha"));
        assert!(!shown.contains("ip_ban"));
    }

    #[test]
    fn a_non_http_redirect_keeps_the_legacy_text() {
        let e = FetchError::NonHttpRedirect("ftp".into());
        assert_eq!(e.to_string(), "blocked redirect to non-http(s) scheme: ftp");
    }
}
