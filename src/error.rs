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
        }
    }
}

impl std::error::Error for FetchError {}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
