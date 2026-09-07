//! Proxy support : the search engine's egress-diversity
//! layer. Residential proxies let each engine see a
//! different IP, each below rate limits.
//!
//! Two protocols: HTTP CONNECT (RFC 7231 §4.3.6) and
//! SOCKS5 (RFC 1928 + RFC 1929 auth). SOCKS5 matters
//! because many residential-proxy providers offer
//! SOCKS5-only lines : and SOCKS5 sends the target host
//! as a domain name so the PROXY resolves DNS, not us
//! (no local DNS leak = stealth-preserving).

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::FetchError;

const PROXY_TIMEOUT: Duration = Duration::from_secs(12);

/// True when `host` matches a NO_PROXY entry. Comma-separated
/// suffix match: "example.com" matches "foo.example.com".
/// "*" disables all proxying.
fn no_proxy_match(host: &str) -> bool {
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if no_proxy.is_empty() {
        return false;
    }
    // The host as delivered is bare ("::1", "example.com"); entries
    // may be bracketed IPv6 ("[::1]"), CIDR ("192.168.0.0/16") or
    // "host:port" (curl 7.86+ supports all of these; E1).
    let host_unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let host_ip: Option<std::net::IpAddr> = host_unbracketed.parse().ok();
    for entry in no_proxy.split(',') {
        let entry = entry.trim();
        if entry == "*" {
            return true;
        }
        // host:port: the port is scoped extra detail; the host part is
        // what matters for matching.
        let entry_no_port = entry
            .rsplit_once(':')
            .filter(|(h, p)| !p.is_empty() && p.parse::<u16>().is_ok() && !h.contains(':'))
            .map(|(h, _)| h)
            .unwrap_or(entry);
        let entry = entry_no_port
            .strip_prefix('[')
            .and_then(|e| e.strip_suffix(']'))
            .unwrap_or(entry_no_port);
        // CIDR: an entry with a prefix length matches hosts whose
        // address falls inside the network.
        if let Some((net, bits)) = entry.split_once('/') {
            if let (Ok(net_ip), Ok(prefix)) = (net.parse::<std::net::IpAddr>(), bits.parse::<u8>())
                && let Some(host_ip) = host_ip
                && cidr_match(host_ip, net_ip, prefix)
            {
                return true;
            }
            continue;
        }
        // Literal IP entry matches a literal IP host exactly (after
        // bracket stripping); a bare IPv6 entry like "::1" also lands
        // on this arm via host_unbracketed == entry.
        if entry.parse::<std::net::IpAddr>().is_ok() && host_ip.is_some() {
            if host_unbracketed == entry {
                return true;
            }
            continue;
        }
        let entry = entry.strip_prefix('.').unwrap_or(entry);
        if host == entry || host_unbracketed == entry {
            return true;
        }
        // host.ends_with(&format!(".{entry}")) without the per-entry
        // allocation: the char before a matching suffix must be '.'.
        // (Byte-identical semantics, including the empty-entry edge.)
        if host.len() > entry.len()
            && host.ends_with(entry)
            && host.as_bytes()[host.len() - entry.len() - 1] == b'.'
        {
            return true;
        }
    }
    false
}

/// Prefix-compare two addresses of the same family (v4 over v6 is
/// never a match).
fn cidr_match(host: std::net::IpAddr, net: std::net::IpAddr, bits: u8) -> bool {
    match (host, net) {
        (std::net::IpAddr::V4(h), std::net::IpAddr::V4(n)) => {
            if bits > 32 {
                return false;
            }
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            (u32::from(h) & mask) == (u32::from(n) & mask)
        }
        (std::net::IpAddr::V6(h), std::net::IpAddr::V6(n)) => {
            if bits > 128 {
                return false;
            }
            let (hb, nb) = (h.octets(), n.octets());
            let full = bits as usize / 8;
            if hb[..full] != nb[..full] {
                return false;
            }
            let rem = bits % 8;
            if rem == 0 {
                return true;
            }
            let mask = u8::MAX << (8 - rem);
            (hb[full] & mask) == (nb[full] & mask)
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    Http,
    Socks5,
}

#[derive(Clone)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub scheme: ProxyScheme,
}

/// Redacts `pass`: a derived Debug would print the plaintext proxy
/// password into any log/error output that formats a `Proxy` with
/// `{:?}`. No such call site exists today, but nothing stops one
/// being added later without anyone noticing the leak.
impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("pass", &"***")
            .field("scheme", &self.scheme)
            .finish()
    }
}

impl Proxy {
    /// Accepts:
    ///   "socks5://user:pass@host:port"
    ///   "http://user:pass@host:port"
    ///   "user:pass@host:port"  (bare = HTTP CONNECT, backward compat)
    ///   "host:port"            (no auth, HTTP CONNECT)
    pub fn parse(s: &str) -> Result<Self, FetchError> {
        // E5: an unsupported scheme ("socks4://host:1080") used to
        // parse as HTTP with the scheme text inside the host, then
        // fail at dial time with a confusing "bad addr" error. Reject
        // any scheme:// line we don't serve, right here, where the
        // user is looking.
        if s.contains("://")
            && let Some(scheme) = s.split("://").next()
            && !scheme.is_empty()
            && !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "socks5" | "socks5h"
            )
        {
            return Err(FetchError::Http(format!(
                "proxy: unsupported scheme '{scheme}://' (supported: http://, socks5://, socks5h://)"
            )));
        }
        let (scheme, rest) = if let Some(r) = s.strip_prefix("socks5://") {
            (ProxyScheme::Socks5, r)
        } else if let Some(r) = s.strip_prefix("socks5h://") {
            (ProxyScheme::Socks5, r) // socks5h = remote DNS (same as our domain ATYP)
        } else if let Some(r) = s.strip_prefix("http://") {
            (ProxyScheme::Http, r)
        } else {
            (ProxyScheme::Http, s)
        };

        // Split auth@addr : auth is optional. The address (host:port)
        // can never contain '@', so split at the LAST one: user and
        // password both may.
        let (user, pass, addr) = match rest.rsplit_once('@') {
            Some((auth, addr)) => {
                let (u, p) = auth
                    .split_once(':')
                    .ok_or_else(|| FetchError::Http(format!("proxy: bad auth in {s}")))?;
                (u.to_string(), p.to_string(), addr)
            }
            None => (String::new(), String::new(), rest),
        };

        let (host, port) = addr
            .rsplit_once(':')
            .ok_or_else(|| FetchError::Http(format!("proxy: bad addr in {s}")))?;
        // rsplit_once handles IPv6 brackets too: [::1]:1080 → ("[::1]", "1080")
        let port: u16 = port
            .parse()
            .map_err(|_| FetchError::Http(format!("proxy: bad port in {s}")))?;
        // Strip IPv6 brackets if present.
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .to_string();

        Ok(Self {
            host,
            port,
            user,
            pass,
            scheme,
        })
    }

    /// Stable id for pool keys and health tracking.
    pub fn id(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// True when traffic goes through an HTTP CONNECT hop: these are
    /// TLS-terminating middleboxes in interception networks, so the
    /// fetch layer switches to the interception-safe handshake.
    pub fn is_http_connect(&self) -> bool {
        self.scheme == ProxyScheme::Http
    }

    /// Raw TCP dial to the proxy itself, for absolute-form plaintext
    /// requests (http:// targets through an HTTP proxy need no tunnel).
    pub async fn connect_tcp(&self) -> Result<TcpStream, FetchError> {
        Ok(tokio::time::timeout(
            PROXY_TIMEOUT,
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map_err(|_| FetchError::Timeout)??)
    }

    /// TCP to the proxy, then tunnel the target through it
    /// via HTTP CONNECT or SOCKS5 depending on scheme.
    pub async fn connect(
        &self,
        target_host: &str,
        target_port: u16,
    ) -> Result<TcpStream, FetchError> {
        let mut stream = self.connect_tcp().await?;
        stream.set_nodelay(true).ok();

        match self.scheme {
            ProxyScheme::Http => {
                self.http_connect(&mut stream, target_host, target_port)
                    .await?
            }
            ProxyScheme::Socks5 => {
                self.socks5_handshake(&mut stream, target_host, target_port)
                    .await?
            }
        }
        Ok(stream)
    }

    /// `Proxy-Authorization` value for this proxy, if credentialed.
    /// CONNECT tunnels and SOCKS5 authenticate in-protocol, but the
    /// raw absolute-form plaintext hop has no tunnel setup to carry
    /// credentials : each request must send this header itself.
    pub fn proxy_authorization(&self) -> Option<String> {
        if self.user.is_empty() {
            return None;
        }
        Some(format!(
            "Basic {}",
            base64(&format!("{}:{}", self.user, self.pass))
        ))
    }

    // ── HTTP CONNECT (RFC 7231 §4.3.6) ──

    async fn http_connect(
        &self,
        stream: &mut TcpStream,
        target_host: &str,
        target_port: u16,
    ) -> Result<(), FetchError> {
        let req = match self.proxy_authorization() {
            None => format!(
                "CONNECT {target_host}:{target_port} HTTP/1.1\r\n\
                 Host: {target_host}:{target_port}\r\n\
                 Proxy-Connection: keep-alive\r\n\r\n"
            ),
            Some(auth) => format!(
                "CONNECT {target_host}:{target_port} HTTP/1.1\r\n\
                 Host: {target_host}:{target_port}\r\n\
                 Proxy-Authorization: {auth}\r\n\
                 Proxy-Connection: keep-alive\r\n\r\n"
            ),
        };
        tokio::time::timeout(PROXY_TIMEOUT, stream.write_all(req.as_bytes()))
            .await
            .map_err(|_| FetchError::Timeout)??;

        // Read the response head (until \r\n\r\n).
        let mut buf = Vec::with_capacity(512);
        let mut byte = [0u8; 1];
        let read_head = async {
            while !buf.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).await? == 0 {
                    return Err(FetchError::Http("proxy: closed during CONNECT".into()));
                }
                buf.push(byte[0]);
                if buf.len() > 4096 {
                    return Err(FetchError::Http("proxy: huge CONNECT response".into()));
                }
            }
            Ok::<(), FetchError>(())
        };
        tokio::time::timeout(PROXY_TIMEOUT, read_head)
            .await
            .map_err(|_| FetchError::Timeout)??;

        let head = String::from_utf8_lossy(&buf);
        let status: u32 = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if status != 200 {
            return Err(FetchError::Http(format!(
                "proxy {} CONNECT -> {status}",
                self.id()
            )));
        }
        Ok(())
    }

    // ── SOCKS5 (RFC 1928 + RFC 1929 auth) ──

    async fn socks5_handshake(
        &self,
        stream: &mut TcpStream,
        target_host: &str,
        target_port: u16,
    ) -> Result<(), FetchError> {
        // Step 1: greeting : offer no-auth (0x00) and if we
        // have credentials, username/password (0x02).
        let has_auth = !self.user.is_empty();
        let methods: &[u8] = if has_auth { &[0x00, 0x02] } else { &[0x00] };
        let greeting = {
            let mut g = vec![0x05, methods.len() as u8];
            g.extend_from_slice(methods);
            g
        };
        tokio::time::timeout(PROXY_TIMEOUT, stream.write_all(&greeting))
            .await
            .map_err(|_| FetchError::Timeout)??;

        // Step 2: server selects a method.
        let mut sel = [0u8; 2];
        tokio::time::timeout(PROXY_TIMEOUT, stream.read_exact(&mut sel))
            .await
            .map_err(|_| FetchError::Timeout)??;
        if sel[0] != 0x05 {
            return Err(FetchError::Http(format!(
                "proxy {} SOCKS5: bad version {}",
                self.id(),
                sel[0]
            )));
        }
        match sel[1] {
            0x00 => {} // no auth needed
            0x02 if has_auth => {
                // RFC 1929: username/password sub-negotiation.
                let user = self.user.as_bytes();
                let pass = self.pass.as_bytes();
                if user.len() > 255 || pass.len() > 255 {
                    return Err(FetchError::Http(format!(
                        "proxy {} SOCKS5: credentials too long",
                        self.id()
                    )));
                }
                let mut auth_req = vec![0x01, user.len() as u8];
                auth_req.extend_from_slice(user);
                auth_req.push(pass.len() as u8);
                auth_req.extend_from_slice(pass);
                tokio::time::timeout(PROXY_TIMEOUT, stream.write_all(&auth_req))
                    .await
                    .map_err(|_| FetchError::Timeout)??;

                let mut auth_resp = [0u8; 2];
                tokio::time::timeout(PROXY_TIMEOUT, stream.read_exact(&mut auth_resp))
                    .await
                    .map_err(|_| FetchError::Timeout)??;
                if auth_resp[1] != 0x00 {
                    return Err(FetchError::Http(format!(
                        "proxy {} SOCKS5: auth failed (status {})",
                        self.id(),
                        auth_resp[1]
                    )));
                }
            }
            0xFF => {
                return Err(FetchError::Http(format!(
                    "proxy {} SOCKS5: no acceptable methods",
                    self.id()
                )));
            }
            other => {
                return Err(FetchError::Http(format!(
                    "proxy {} SOCKS5: unsupported method {:#04x}",
                    self.id(),
                    other
                )));
            }
        }

        // Step 3: CONNECT request. We send the target as a
        // DOMAIN NAME (ATYP 0x03) so the proxy resolves DNS
        // : no local DNS leak, stealth-preserving.
        let host_bytes = target_host.as_bytes();
        if host_bytes.len() > 255 {
            return Err(FetchError::Http(format!(
                "proxy {} SOCKS5: hostname too long",
                self.id()
            )));
        }
        let mut req = vec![
            0x05, // VER
            0x01, // CMD = CONNECT
            0x00, // RSV
            0x03, // ATYP = domain name
        ];
        req.push(host_bytes.len() as u8);
        req.extend_from_slice(host_bytes);
        req.extend_from_slice(&target_port.to_be_bytes());
        tokio::time::timeout(PROXY_TIMEOUT, stream.write_all(&req))
            .await
            .map_err(|_| FetchError::Timeout)??;

        // Step 4: server reply.
        // VER(1) | REP(1) | RSV(1) | ATYP(1) | BND.ADDR(variable) | BND.PORT(2)
        let mut header = [0u8; 4];
        tokio::time::timeout(PROXY_TIMEOUT, stream.read_exact(&mut header))
            .await
            .map_err(|_| FetchError::Timeout)??;
        if header[0] != 0x05 {
            return Err(FetchError::Http(format!(
                "proxy {} SOCKS5: bad reply version {}",
                self.id(),
                header[0]
            )));
        }
        if header[1] != 0x00 {
            // Map RFC 1928 reply codes to readable errors.
            let reason = match header[1] {
                0x01 => "general failure",
                0x02 => "connection not allowed",
                0x03 => "network unreachable",
                0x04 => "host unreachable",
                0x05 => "connection refused",
                0x06 => "TTL expired",
                0x07 => "command not supported",
                0x08 => "address type not supported",
                code => {
                    return Err(FetchError::Http(format!(
                        "proxy {} SOCKS5: reply error {:#04x}",
                        self.id(),
                        code
                    )));
                }
            };
            return Err(FetchError::Http(format!(
                "proxy {} SOCKS5: {reason}",
                self.id()
            )));
        }

        // Skip BND.ADDR + BND.PORT : we don't need the
        // bound address, just consume it so the stream is
        // clean for the caller's TLS handshake.
        let addr_len = match header[3] {
            0x01 => 4, // IPv4
            0x03 => {
                // domain: read 1 length byte, then that many
                let mut len = [0u8; 1];
                tokio::time::timeout(PROXY_TIMEOUT, stream.read_exact(&mut len))
                    .await
                    .map_err(|_| FetchError::Timeout)??;
                len[0] as usize
            }
            0x04 => 16, // IPv6
            other => {
                return Err(FetchError::Http(format!(
                    "proxy {} SOCKS5: bad ATYP {:#04x} in reply",
                    self.id(),
                    other
                )));
            }
        };
        // For domain ATYP we already consumed the length byte
        // above; for IPv4/IPv6 addr_len is the full address.
        let mut discard = vec![0u8; addr_len + 2]; // +2 for BND.PORT
        tokio::time::timeout(PROXY_TIMEOUT, stream.read_exact(&mut discard))
            .await
            .map_err(|_| FetchError::Timeout)??;

        Ok(())
    }

    /// Reconstruct the proxy URL string from parsed fields.
    /// Handles IPv6 bracketing. Used for config-file round-trip.
    pub fn to_url(&self) -> String {
        let scheme = scheme_str(self.scheme);
        let host = bracketed_host(&self.host);
        if self.user.is_empty() {
            format!("{scheme}://{host}:{}", self.port)
        } else {
            format!(
                "{scheme}://{}:{}@{host}:{}",
                self.user, self.pass, self.port
            )
        }
    }

    /// Chrome-compatible `--proxy-server` value (scheme://host:port, no
    /// credentials : Chrome handles proxy auth via its own dialog or
    /// `--proxy-auth` extension). Used for the Ghost browser tier.
    pub fn chrome_proxy_arg(&self) -> String {
        format!(
            "{}://{}:{}",
            scheme_str(self.scheme),
            bracketed_host(&self.host),
            self.port
        )
    }
}

fn scheme_str(scheme: ProxyScheme) -> &'static str {
    match scheme {
        ProxyScheme::Http => "http",
        ProxyScheme::Socks5 => "socks5",
    }
}

fn bracketed_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

// ── Config file ──────────────────────────────────────────────
//
// The proxy config is a plain text file (one URL per line, #
// comments, blanks ignored) at cache_dir/proxies.txt. The MCP
// server and search engine read it at startup via `load_all()`,
// which merges the file with DONSEEK_PROXIES (env overrides file
// for duplicate host:port).

/// Path to the proxy config file.
pub fn config_path() -> PathBuf {
    crate::paths::cache_dir().join("proxies.txt")
}

/// Load proxies from the config file. Returns empty vec if the
/// file doesn't exist (not an error : first run).
pub fn load_config() -> Vec<Proxy> {
    load_config_verbose().0
}

/// Same as `load_config` but reports how many non-comment lines were
/// dropped as unparseable (Q1: a typo in proxies.txt used to mean a
/// silently absent proxy). `(proxies, skipped)`.
pub fn load_config_verbose() -> (Vec<Proxy>, usize) {
    let path = config_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return (Vec::new(), 0);
    };
    parse_lines_verbose(&content)
}

/// Load proxies from `DONSEEK_PROXIES` env var (comma-separated).
pub fn load_env() -> Vec<Proxy> {
    let raw = std::env::var("DONSEEK_PROXIES").unwrap_or_default();
    raw.split(',')
        .filter_map(|s| Proxy::parse(s.trim()).ok())
        .collect()
}

/// Load all proxies: config file first, then env var overrides
/// duplicates by host:port. This is what the MCP server and
/// search engine call at startup.
pub fn load_all() -> Vec<Proxy> {
    let mut proxies = load_config();
    for ep in load_env() {
        if let Some(pos) = proxies.iter().position(|p| p.id() == ep.id()) {
            proxies[pos] = ep;
        } else {
            proxies.push(ep);
        }
    }
    proxies
}

/// Detect a proxy from standard environment variables for a given URL.
/// Checks in order: HTTPS_PROXY (for https://), HTTP_PROXY (for http://),
/// ALL_PROXY (both). Also checks lowercase variants. NO_PROXY is respected:
/// comma-separated host suffixes that bypass the proxy.
///
/// This follows the curl/wget convention, so `HTTP_PROXY=http://proxy:8080`
/// works out of the box. SOCKS5 proxies via `ALL_PROXY=socks5://host:port`
/// are also supported.
pub fn from_env_for(url: &str) -> Option<Proxy> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;

    // NO_PROXY bypass.
    if no_proxy_match(host) {
        return None;
    }

    // Scheme-specific env var, then ALL_PROXY as fallback.
    // Check uppercase first, then lowercase (curl convention).
    // Note (Q2): non-http(s) schemes fall into the HTTP_PROXY arm. DonSeTch
    // never dials non-http(s) URLs (the URL gate rejects them first), so
    // curl's "ALL_PROXY covers unknown schemes" rule is dormant here; the
    // ALL_PROXY fallback below already covers both http and https.
    let env_name = if scheme == "https" {
        "HTTPS_PROXY"
    } else {
        "HTTP_PROXY"
    };
    let env_val = std::env::var(env_name)
        .or_else(|_| std::env::var(env_name.to_lowercase()))
        .or_else(|_| std::env::var("ALL_PROXY"))
        .or_else(|_| std::env::var("all_proxy"))
        .ok()?;
    let env_val = env_val.trim();
    if env_val.is_empty() {
        return None;
    }
    Proxy::parse(env_val).ok()
}

/// Save proxies to the config file. Atomic write (temp + rename).
/// Sets 0600 on Unix (credentials present).
pub fn save_config(proxies: &[Proxy]) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = String::from("# DonSeTch proxy configuration\n");
    for p in proxies {
        content.push_str(&p.to_url());
        content.push('\n');
    }
    let tmp = path.with_extension("txt.tmp");
    // 0600 from creation: write-then-chmod left `proxies.txt.tmp`
    // (passwords inside) world-readable between the two calls, and
    // permanently on a crash in between.
    crate::config::write_private(&tmp, content.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Parse proxy URLs from text: one per line, # comments and
/// blank lines ignored.
fn parse_lines_verbose(content: &str) -> (Vec<Proxy>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match Proxy::parse(line) {
            Ok(p) => out.push(p),
            Err(_) => skipped += 1,
        }
    }
    (out, skipped)
}

pub(crate) fn base64(input: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(input.as_bytes())
}

#[cfg(test)]
mod tests {

    // E1: IPv6 literals (bracketed and bare), CIDR networks and
    // host:port entries in NO_PROXY.
    #[test]
    fn no_proxy_matches_ipv6_cidr_and_port_entries() {
        unsafe {
            std::env::set_var(
                "NO_PROXY",
                "[::1],192.168.0.0/16,example.com:8443,.internal.local",
            )
        };
        assert!(no_proxy_match("::1"), "bare IPv6 host vs bracketed entry");
        assert!(no_proxy_match("192.168.5.5"), "CIDR /16");
        assert!(!no_proxy_match("192.169.0.1"), "outside the CIDR");
        assert!(
            no_proxy_match("example.com"),
            "host:port entry matches the host"
        );
        assert!(!no_proxy_match("example.org"), "other hosts unaffected");
        assert!(
            no_proxy_match("api.internal.local"),
            "dot-prefixed entry still matches"
        );
        unsafe { std::env::remove_var("NO_PROXY") };
    }

    #[test]
    fn no_proxy_cidr_v6() {
        unsafe { std::env::set_var("NO_PROXY", "fc00::/7") };
        assert!(no_proxy_match("fc00:1::2"), "inside fc00::/7");
        assert!(!no_proxy_match("fe80::1"), "outside");
        unsafe { std::env::remove_var("NO_PROXY") };
    }

    // E5: an unsupported scheme line is a loud parse error, not a
    // proxy that pretends to be HTTP and dies at dial time.
    #[test]
    fn proxy_parse_rejects_unsupported_schemes() {
        assert!(Proxy::parse("socks4://127.0.0.1:1080").is_err());
        assert!(Proxy::parse("https://127.0.0.1:3128").is_err());
        assert!(Proxy::parse("ftp://127.0.0.1:21").is_err());
        // supported schemes still parse
        assert!(Proxy::parse("http://127.0.0.1:3128").is_ok());
        assert!(Proxy::parse("socks5://127.0.0.1:1080").is_ok());
        assert!(Proxy::parse("socks5h://127.0.0.1:1080").is_ok());
        // scheme-less lines keep their legacy meaning (http by default)
        assert!(Proxy::parse("127.0.0.1:3128").is_ok());
    }

    // Q1: unparseable proxy lines are counted, not silently dropped.
    #[test]
    fn parse_lines_verbose_counts_skipped() {
        let (proxies, skipped) = parse_lines_verbose(
            "http://127.0.0.1:3128\n\n# comment\nsocks4://127.0.0.1:1080\ngarbage-no-colon\n",
        );
        assert_eq!(proxies.len(), 1);
        assert_eq!(skipped, 2, "socks4 line + garbage line counted");
    }
    use super::*;

    #[test]
    fn debug_redacts_password() {
        let p = Proxy {
            host: "proxy.example.com".into(),
            port: 8080,
            user: "alice".into(),
            pass: "s3cret-password".into(),
            scheme: ProxyScheme::Http,
        };
        let out = format!("{p:?}");
        assert!(!out.contains("s3cret-password"), "leaked password: {out}");
        assert!(out.contains("proxy.example.com"));
        assert!(out.contains("alice"));
        assert!(out.contains("***"));
    }

    #[test]
    fn base64_rfc4648_vectors() {
        // RFC 4648 §10 : covers every input-length remainder.
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("foob"), "Zm9vYg==");
        assert_eq!(base64("fooba"), "Zm9vYmE=");
        assert_eq!(base64("foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_credentials_pad_at_the_end() {
        // Regression: the final partial group was emitted as padding
        // first, data after : "dXNlcjpwYXNz==QA" instead of
        // "dXNlcjpwYXNzd2Q=". Only credentials whose length was an exact
        // multiple of 3 survived, so most basic-auth and proxy-auth
        // headers went out corrupted.
        assert_eq!(base64("user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64("user:passw"), "dXNlcjpwYXNzdw==");
        assert_eq!(base64("user:passwd"), "dXNlcjpwYXNzd2Q=");
    }

    #[test]
    fn parse_bare_http() {
        let p = Proxy::parse("user:pass@1.2.3.4:8080").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Http);
        assert_eq!(p.host, "1.2.3.4");
        assert_eq!(p.port, 8080);
        assert_eq!(p.user, "user");
        assert_eq!(p.pass, "pass");
    }

    #[test]
    fn parse_explicit_http() {
        let p = Proxy::parse("http://u:p@host:3128").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Http);
        assert_eq!(p.host, "host");
        assert_eq!(p.port, 3128);
    }

    #[test]
    fn parse_socks5() {
        let p = Proxy::parse("socks5://u:p@5.6.7.8:1080").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Socks5);
        assert_eq!(p.host, "5.6.7.8");
        assert_eq!(p.port, 1080);
        assert_eq!(p.user, "u");
        assert_eq!(p.pass, "p");
    }

    #[test]
    fn parse_socks5h_alias() {
        let p = Proxy::parse("socks5h://u:p@host:1080").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Socks5);
    }

    #[test]
    fn parse_socks5_no_auth() {
        let p = Proxy::parse("socks5://host:1080").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Socks5);
        assert_eq!(p.user, "");
        assert_eq!(p.pass, "");
    }

    #[test]
    fn parse_http_no_auth() {
        let p = Proxy::parse("host:8080").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Http);
        assert_eq!(p.host, "host");
        assert_eq!(p.user, "");
    }

    #[test]
    fn parse_ipv6_brackets() {
        let p = Proxy::parse("socks5://u:p@[::1]:1080").unwrap();
        assert_eq!(p.host, "::1");
        assert_eq!(p.port, 1080);
    }

    #[test]
    fn id_stable() {
        let p = Proxy::parse("socks5://u:p@host:1080").unwrap();
        assert_eq!(p.id(), "host:1080");
    }

    #[test]
    fn parse_bad() {
        assert!(Proxy::parse("garbage").is_err());
        assert!(Proxy::parse("u:p@bad").is_err());
        assert!(Proxy::parse("u:p@host:99999").is_err());
    }

    // Auth was split from the address at the FIRST '@', so a
    // password containing one ("p@ss") ended up as pass="p" and
    // host="ss@1.2.3.4": accepted by `proxy add`, stored, and then
    // unreachable. The address can never contain '@' (host:port),
    // so the LAST '@' is the only correct split point.
    #[test]
    fn parse_password_containing_at() {
        let p = Proxy::parse("socks5://alice:p@ss@1.2.3.4:1080").unwrap();
        assert_eq!(p.user, "alice");
        assert_eq!(p.pass, "p@ss");
        assert_eq!(p.host, "1.2.3.4");
        assert_eq!(p.port, 1080);
        // Round trip through to_url keeps the password whole.
        let p2 = Proxy::parse(&p.to_url()).unwrap();
        assert_eq!(p2.pass, "p@ss");
        assert_eq!(p2.host, "1.2.3.4");
    }

    #[test]
    fn parse_user_and_password_containing_at_with_ipv6() {
        let p = Proxy::parse("http://me@corp:p@ss:w0rd@[::1]:3128").unwrap();
        assert_eq!(p.user, "me@corp");
        assert_eq!(p.pass, "p@ss:w0rd");
        assert_eq!(p.host, "::1");
        assert_eq!(p.port, 3128);
        let p2 = Proxy::parse(&p.to_url()).unwrap();
        assert_eq!(p2.user, "me@corp");
        assert_eq!(p2.pass, "p@ss:w0rd");
        assert_eq!(p2.host, "::1");
    }

    #[test]
    fn to_url_roundtrip() {
        let urls = [
            "socks5://user:pass@host:1080",
            "http://u:p@1.2.3.4:8080",
            "socks5://host:1080",
            "http://host:8080",
            "user:pass@host:8080",
            "host:8080",
        ];
        for url in urls {
            let p = Proxy::parse(url).unwrap();
            let reconstructed = p.to_url();
            let p2 = Proxy::parse(&reconstructed).unwrap();
            assert_eq!(p.scheme, p2.scheme, "scheme mismatch for {url}");
            assert_eq!(p.host, p2.host, "host mismatch for {url}");
            assert_eq!(p.port, p2.port, "port mismatch for {url}");
            assert_eq!(p.user, p2.user, "user mismatch for {url}");
            assert_eq!(p.pass, p2.pass, "pass mismatch for {url}");
        }
    }

    #[test]
    fn to_url_ipv6_brackets() {
        let p = Proxy::parse("socks5://u:p@[::1]:1080").unwrap();
        let url = p.to_url();
        assert!(
            url.contains("[::1]"),
            "IPv6 host should be bracketed: {url}"
        );
        let p2 = Proxy::parse(&url).unwrap();
        assert_eq!(p.host, p2.host);
        assert_eq!(p.port, p2.port);
    }

    #[test]
    fn parse_lines_ignores_comments_and_blanks() {
        let content = "\
# This is a comment
socks5://u:p@host:1080

  # Indented comment
http://host:8080

# Empty line above
";
        let proxies = parse_lines_verbose(content).0;
        assert_eq!(proxies.len(), 2);
        assert_eq!(proxies[0].id(), "host:1080");
        assert_eq!(proxies[1].id(), "host:8080");
    }

    #[test]
    fn parse_lines_skips_invalid() {
        let content = "\
socks5://valid:1080
garbage_line
u:p@also_valid:8080
:99999
";
        let proxies = parse_lines_verbose(content).0;
        assert_eq!(proxies.len(), 2);
    }

    #[test]
    fn parse_lines_empty() {
        assert!(parse_lines_verbose("").0.is_empty());
        assert!(
            parse_lines_verbose("# only comments\n# more comments")
                .0
                .is_empty()
        );
        assert!(parse_lines_verbose("\n\n\n").0.is_empty());
    }

    // ── from_env_for tests ──
    // These tests use std::env::set_var which is not thread-safe,
    // so each test sets and cleans up its own vars. Rust's test runner
    // runs tests in parallel by default, but these tests use unique
    // var names to avoid collisions. The standard proxy vars
    // (HTTP_PROXY etc.) are cleaned up after each test.
    //
    // SAFETY: set_var/remove_var are unsafe in Rust 2024 edition because
    // they're not thread-safe. We guard all env var tests with a mutex to
    // serialize them, so only one test touches env vars at a time.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn env_https_proxy() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::remove_var("NO_PROXY");
            std::env::remove_var("no_proxy");
            std::env::set_var("HTTPS_PROXY", "http://proxy:8080");
        }
        let p = from_env_for("https://example.com/").expect("should detect proxy");
        assert_eq!(p.host, "proxy");
        assert_eq!(p.port, 8080);
        assert_eq!(p.scheme, ProxyScheme::Http);
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
        }
    }

    #[test]
    fn env_http_proxy() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::remove_var("NO_PROXY");
            std::env::remove_var("no_proxy");
            std::env::set_var("HTTP_PROXY", "http://proxy:3128");
        }
        let p = from_env_for("http://example.com/").expect("should detect proxy");
        assert_eq!(p.host, "proxy");
        assert_eq!(p.port, 3128);
        unsafe {
            std::env::remove_var("HTTP_PROXY");
        }
    }

    #[test]
    fn env_all_proxy_fallback() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("HTTP_PROXY");
            std::env::remove_var("https_proxy");
            std::env::remove_var("http_proxy");
            std::env::set_var("ALL_PROXY", "socks5://proxy:1080");
        }
        let p = from_env_for("https://example.com/").expect("should detect proxy");
        assert_eq!(p.host, "proxy");
        assert_eq!(p.port, 1080);
        assert_eq!(p.scheme, ProxyScheme::Socks5);
        unsafe {
            std::env::remove_var("ALL_PROXY");
        }
    }

    #[test]
    fn env_lowercase_proxy() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::remove_var("NO_PROXY");
            std::env::remove_var("no_proxy");
            std::env::remove_var("HTTPS_PROXY");
            std::env::set_var("https_proxy", "http://proxy:8080");
        }
        let p = from_env_for("https://example.com/").expect("should detect lowercase proxy");
        assert_eq!(p.host, "proxy");
        unsafe {
            std::env::remove_var("https_proxy");
        }
    }

    #[test]
    fn env_no_proxy_bypass() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://proxy:8080");
            std::env::set_var("NO_PROXY", "example.com");
        }
        assert!(
            from_env_for("https://example.com/").is_none(),
            "NO_PROXY should bypass"
        );
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("NO_PROXY");
        }
    }

    #[test]
    fn env_no_proxy_wildcard() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://proxy:8080");
            std::env::set_var("NO_PROXY", "*");
        }
        assert!(
            from_env_for("https://example.com/").is_none(),
            "NO_PROXY=* should bypass all"
        );
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("NO_PROXY");
        }
    }

    #[test]
    fn env_no_proxy_subdomain() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://proxy:8080");
            std::env::set_var("NO_PROXY", ".example.com");
        }
        assert!(
            from_env_for("https://foo.example.com/").is_none(),
            "NO_PROXY=.example.com should match subdomain"
        );
        assert!(
            from_env_for("https://example.com/").is_none(),
            "NO_PROXY=.example.com should match root"
        );
        assert!(
            from_env_for("https://other.com/").is_some(),
            "NO_PROXY should not match unrelated domain"
        );
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("NO_PROXY");
        }
    }

    #[test]
    fn env_no_proxy_returns_none_without_env() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("HTTP_PROXY");
            std::env::remove_var("ALL_PROXY");
            std::env::remove_var("https_proxy");
            std::env::remove_var("http_proxy");
            std::env::remove_var("all_proxy");
        }
        assert!(
            from_env_for("https://example.com/").is_none(),
            "no env vars = no proxy"
        );
    }

    #[test]
    fn chrome_proxy_arg_format() {
        let p = Proxy::parse("socks5://u:p@host:1080").unwrap();
        let arg = p.chrome_proxy_arg();
        assert_eq!(arg, "socks5://host:1080");
        assert!(!arg.contains("u:p"));
    }
}
