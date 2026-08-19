//! Proxy support — the search engine's egress-diversity
//! layer. Residential proxies let each engine see a
//! different IP, each below rate limits.
//!
//! Two protocols: HTTP CONNECT (RFC 7231 §4.3.6) and
//! SOCKS5 (RFC 1928 + RFC 1929 auth). SOCKS5 matters
//! because many residential-proxy providers offer
//! SOCKS5-only lines — and SOCKS5 sends the target host
//! as a domain name so the PROXY resolves DNS, not us
//! (no local DNS leak = stealth-preserving).

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::FetchError;

const PROXY_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    Http,
    Socks5,
}

#[derive(Debug, Clone)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub scheme: ProxyScheme,
}

impl Proxy {
    /// Accepts:
    ///   "socks5://user:pass@host:port"
    ///   "http://user:pass@host:port"
    ///   "user:pass@host:port"  (bare = HTTP CONNECT, backward compat)
    ///   "host:port"            (no auth, HTTP CONNECT)
    pub fn parse(s: &str) -> Result<Self, FetchError> {
        let (scheme, rest) = if let Some(r) = s.strip_prefix("socks5://") {
            (ProxyScheme::Socks5, r)
        } else if let Some(r) = s.strip_prefix("socks5h://") {
            (ProxyScheme::Socks5, r) // socks5h = remote DNS (same as our domain ATYP)
        } else if let Some(r) = s.strip_prefix("http://") {
            (ProxyScheme::Http, r)
        } else {
            (ProxyScheme::Http, s)
        };

        // Split auth@addr — auth is optional.
        let (user, pass, addr) = match rest.split_once('@') {
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

    /// TCP to the proxy, then tunnel the target through it
    /// via HTTP CONNECT or SOCKS5 depending on scheme.
    pub async fn connect(
        &self,
        target_host: &str,
        target_port: u16,
    ) -> Result<TcpStream, FetchError> {
        let mut stream = tokio::time::timeout(
            PROXY_TIMEOUT,
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map_err(|_| FetchError::Timeout)??;
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

    // ── HTTP CONNECT (RFC 7231 §4.3.6) ──

    async fn http_connect(
        &self,
        stream: &mut TcpStream,
        target_host: &str,
        target_port: u16,
    ) -> Result<(), FetchError> {
        let req = if self.user.is_empty() {
            format!(
                "CONNECT {target_host}:{target_port} HTTP/1.1\r\n\
                 Host: {target_host}:{target_port}\r\n\
                 Proxy-Connection: keep-alive\r\n\r\n"
            )
        } else {
            let auth = base64(&format!("{}:{}", self.user, self.pass));
            format!(
                "CONNECT {target_host}:{target_port} HTTP/1.1\r\n\
                 Host: {target_host}:{target_port}\r\n\
                 Proxy-Authorization: Basic {auth}\r\n\
                 Proxy-Connection: keep-alive\r\n\r\n"
            )
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
        // Step 1: greeting — offer no-auth (0x00) and if we
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
        // — no local DNS leak, stealth-preserving.
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

        // Skip BND.ADDR + BND.PORT — we don't need the
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
        let scheme = match self.scheme {
            ProxyScheme::Http => "http",
            ProxyScheme::Socks5 => "socks5",
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
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
    /// credentials — Chrome handles proxy auth via its own dialog or
    /// `--proxy-auth` extension). Used for the Ghost browser tier.
    pub fn chrome_proxy_arg(&self) -> String {
        let scheme = match self.scheme {
            ProxyScheme::Http => "http",
            ProxyScheme::Socks5 => "socks5",
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{scheme}://{host}:{}", self.port)
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
/// file doesn't exist (not an error — first run).
pub fn load_config() -> Vec<Proxy> {
    let path = config_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_lines(&content)
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

    // NO_PROXY: comma-separated host suffixes that bypass the proxy.
    // Matches the host exactly or as a suffix (e.g. ".example.com"
    // matches "foo.example.com"). Also "*" disables all proxying.
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if !no_proxy.is_empty() {
        for entry in no_proxy.split(',') {
            let entry = entry.trim();
            if entry == "*" {
                return None;
            }
            let entry = entry.strip_prefix('.').unwrap_or(entry);
            if host == entry || host.ends_with(&format!(".{entry}")) {
                return None;
            }
        }
    }

    // Scheme-specific env var, then ALL_PROXY as fallback.
    // Check uppercase first, then lowercase (curl convention).
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
    std::fs::write(&tmp, &content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Parse proxy URLs from text: one per line, # comments and
/// blank lines ignored.
fn parse_lines(content: &str) -> Vec<Proxy> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| Proxy::parse(line).ok())
        .collect()
}

pub(crate) fn base64(input: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = input.as_bytes();
    let mut out = String::with_capacity(b.len() * 4 / 3 + 4);
    for chunk in b.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &c)| acc | ((c as u32) << (16 - 8 * i)));
        for i in 0..4 {
            let shift = 18 - 6 * i;
            // Padding goes at the END: output char `i` covers bits
            // [i*6, i*6+6). It is padding only when the chunk has no
            // bits that far in. Testing `shift` instead gets this
            // backwards, since shift counts down as i counts up.
            let pad = i * 6 >= chunk.len() * 8;
            out.push(if pad {
                '='
            } else {
                T[((n >> shift) & 63) as usize] as char
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc4648_vectors() {
        // RFC 4648 §10 — covers every input-length remainder.
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
        // first, data after — "dXNlcjpwYXNz==QA" instead of
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
        let proxies = parse_lines(content);
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
        let proxies = parse_lines(content);
        assert_eq!(proxies.len(), 2);
    }

    #[test]
    fn parse_lines_empty() {
        assert!(parse_lines("").is_empty());
        assert!(parse_lines("# only comments\n# more comments").is_empty());
        assert!(parse_lines("\n\n\n").is_empty());
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
