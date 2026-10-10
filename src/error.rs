use std::fmt;

#[derive(Debug)]
pub enum FetchError {
    InvalidUrl(String),
    Tls(String),
    Io(std::io::Error),
    Http(String),
    /// Invalid local proxy settings, rather than an origin transport failure.
    ProxyConfig(String),
    Ghost(String),
    Timeout,
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
    pub fn ghost(msg: impl Into<String>) -> Self {
        Self::Ghost(msg.into())
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
            Self::Ghost(e) => write!(f, "ghost: {e}"),
            Self::Timeout => write!(f, "timeout"),
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
            Self::Denied { .. } => write!(f, "blocked by a local DonSeTch rule"),
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
        assert_eq!(shown, "blocked by a local DonSeTch rule");
        assert!(!shown.contains("facebook") && !shown.contains("captcha"));
        assert!(!shown.contains("ip_ban"));
    }

    #[test]
    fn a_non_http_redirect_keeps_the_legacy_text() {
        let e = FetchError::NonHttpRedirect("ftp".into());
        assert_eq!(e.to_string(), "blocked redirect to non-http(s) scheme: ftp");
    }
}
