//! Minimal RFC 6265 cookie jar, scoped per domain/path.
//! Tracks real expiry (Max-Age → expires_at) for the self-
//! improving fetch loop's cookie write-back.

use crate::ghost::cache::CookieRecord;

#[derive(Clone, Debug)]
pub struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    host_only: bool,
    /// Unix-seconds expiry. None = session cookie.
    expires_at: Option<u64>,
}

#[derive(Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

fn normalize_domain(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Reject control characters (CR, LF, NUL and other ASCII controls)
    if trimmed
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == b'\0' || b < 0x20 || b == 0x7F)
    {
        return None;
    }
    let mut s = trimmed.to_ascii_lowercase();
    // Strip one leading dot (RFC 6265 allows a leading dot, but only one is significant)
    if s.starts_with('.') {
        s = s[1..].to_string();
    }
    // Strip one trailing root dot (e.g. "example.com.")
    if s.ends_with('.') {
        s.pop();
    }
    if s.is_empty() {
        return None;
    }
    if s.bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == b'\0' || b < 0x20 || b == 0x7F)
    {
        return None;
    }
    if s.len() > 253 {
        return None;
    }
    for label in s.split('.') {
        if label.is_empty() {
            return None;
        }
        if label.len() > 63 {
            return None;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return None;
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return None;
        }
    }
    Some(s)
}

fn normalize_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == b'\0' || b < 0x20 || b == 0x7F)
    {
        return None;
    }
    let mut s = trimmed.to_ascii_lowercase();
    // Strip one trailing root dot for comparison (hosts are sent without it)
    if s.ends_with('.') {
        s.pop();
    }
    if s.is_empty() {
        return None;
    }
    if s.len() > 253 {
        return None;
    }
    for label in s.split('.') {
        if label.is_empty() {
            return None;
        }
        if label.len() > 63 {
            return None;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return None;
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return None;
        }
    }
    Some(s)
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store all Set-Cookie headers from a response for `host`.
    pub fn store_from_headers(&mut self, host: &str, headers: &[(String, String)]) {
        let Some(normalized_host) = normalize_host(host) else {
            return;
        };
        for (n, v) in headers {
            if !n.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            let mut parts = v.split(';');
            let Some(pair) = parts.next() else { continue };
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if name.is_empty() {
                continue;
            }
            // Control characters in name/value can split the
            // Cookie request header later (request splitting).
            // Reject the cookie outright.
            if name.contains(['\r', '\n', '\0']) || value.contains(['\r', '\n', '\0']) {
                continue;
            }
            let mut domain = normalized_host.clone();
            let mut host_only = true;
            let mut path = "/".to_string();
            let mut expired = false;
            let mut expires_at: Option<u64> = None;
            for attr in parts {
                let attr = attr.trim();
                if let Some((k, val)) = attr.split_once('=') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "domain" => {
                            let Some(normalized) = normalize_domain(val.trim()) else {
                                continue;
                            };
                            // Reject public suffixes (e.g. "com", "co.uk")
                            // psl::domain is None for public suffixes, Some for registrable domains
                            if psl::domain(normalized.as_bytes()).is_none() {
                                continue;
                            }
                            // RFC 6265 §5.3 step 6: reject Domain
                            // attributes that are not the request
                            // host or a parent of it — otherwise any
                            // origin can pin cookies on any victim
                            // domain (cookie tossing).
                            if normalized == normalized_host
                                || normalized_host.ends_with(&format!(".{normalized}"))
                            {
                                domain = normalized;
                                host_only = false;
                            }
                        }
                        "path" => path = val.trim().to_string(),
                        "max-age" => {
                            let secs: i64 = val.trim().parse().unwrap_or(1);
                            if secs <= 0 {
                                expired = true;
                            } else {
                                expires_at = Some(
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs())
                                        .unwrap_or(0)
                                        + secs as u64,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            // Replace any existing cookie with same (name, domain, path).
            self.cookies
                .retain(|c| !(c.name == name && c.domain == domain && c.path == path));
            if !expired {
                self.cookies.push(Cookie {
                    name,
                    value,
                    domain,
                    path,
                    host_only,
                    expires_at,
                });
            }
        }
        self.purge_expired();
    }

    /// Inject a cookie harvested out-of-band (DonGhost
    /// clearance handoff). Leading-dot domains are
    /// subdomain cookies; bare domains are host-only.
    /// `expires_at` carries the real CDP expiry.
    pub fn store_raw(&mut self, name: &str, value: &str, domain: &str, expires_at: Option<u64>) {
        // Same control-character rejection as store_from_headers:
        // CDP-harvested values must never split the Cookie header.
        if name.contains(['\r', '\n', '\0']) || value.contains(['\r', '\n', '\0']) {
            return;
        }
        // Preserve leading-dot subdomain semantics
        let is_subdomain = domain.trim().starts_with('.');
        let Some(normalized) = normalize_domain(domain) else {
            return;
        };
        // Reject invalid/public-suffix domains
        if psl::domain(normalized.as_bytes()).is_none() {
            return;
        }
        let host_only = !is_subdomain;
        self.cookies
            .retain(|c| !(c.name == name && c.domain == normalized && c.path == "/"));
        self.cookies.push(Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: normalized,
            path: "/".into(),
            host_only,
            expires_at,
        });
    }

    /// Export all cookies matching `host` as CookieRecords
    /// for write-back to the persistent domain profile.
    pub fn snapshot_for(&self, host: &str) -> Vec<CookieRecord> {
        let Some(normalized_host) = normalize_host(host) else {
            return Vec::new();
        };
        let now = now_secs();
        self.cookies
            .iter()
            .filter(|c| c.expires_at.is_none_or(|e| e > now))
            .filter(|c| {
                if c.host_only {
                    normalized_host == c.domain
                } else {
                    normalized_host == c.domain
                        || normalized_host.ends_with(&format!(".{}", c.domain))
                }
            })
            .map(|c| CookieRecord {
                name: c.name.clone(),
                value: c.value.clone(),
                domain: c.domain.clone(),
                path: "/".to_string(),
                expires_at: c.expires_at,
                secure: false,
                http_only: false,
                same_site: "Lax".to_string(),
            })
            .collect()
    }

    /// Cookie header value for a request to `host` + `path`, if any match.
    pub fn header_for(&self, host: &str, path: &str) -> Option<String> {
        let normalized_host = normalize_host(host)?;
        let now = now_secs();
        let mut pairs: Vec<&Cookie> = Vec::new();
        for c in &self.cookies {
            // Session cookies (no expiry) always match; expired
            // cookies must never be replayed.
            if c.expires_at.is_some_and(|e| e <= now) {
                continue;
            }
            let domain_ok = if c.host_only {
                normalized_host == c.domain
            } else {
                normalized_host == c.domain || normalized_host.ends_with(&format!(".{}", c.domain))
            };
            // RFC 6265 §5.1.4 path-match: exact, or prefix followed
            // by '/' (a /foo cookie must not match /foobar).
            let path_ok = path == c.path
                || (path.starts_with(&c.path)
                    && (c.path.ends_with('/') || path.as_bytes().get(c.path.len()) == Some(&b'/')));
            if domain_ok && path_ok {
                pairs.push(c);
            }
        }
        if pairs.is_empty() {
            return None;
        }
        // Longest path first, per RFC 6265 §5.4.
        pairs.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        Some(
            pairs
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Drop cookies whose expiry has passed.
    pub fn purge_expired(&mut self) {
        let now = now_secs();
        self.cookies
            .retain(|c| c.expires_at.is_none_or(|e| e > now));
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_com_not_shared() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[("Set-Cookie".to_string(), "a=1; Domain=com".to_string())],
        );
        // Public suffix should be rejected -> fallback to host-only
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.com").len(), 0);
        assert_eq!(jar.snapshot_for("evil.com").len(), 0);
        assert!(jar.header_for("example.com", "/").is_some());
        assert!(jar.header_for("sub.example.com", "/").is_none());
        assert!(jar.header_for("other.com", "/").is_none());
    }

    #[test]
    fn domain_co_uk_not_shared() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.co.uk",
            &[("Set-Cookie".to_string(), "a=1; Domain=co.uk".to_string())],
        );
        assert_eq!(jar.snapshot_for("example.co.uk").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.co.uk").len(), 0);
        assert_eq!(jar.snapshot_for("evil.co.uk").len(), 0);
        assert!(jar.header_for("example.co.uk", "/").is_some());
        assert!(jar.header_for("sub.example.co.uk", "/").is_none());
    }

    #[test]
    fn host_only_exact_match() {
        let mut jar = CookieJar::new();
        jar.store_raw("a", "1", "example.com", None);
        assert!(jar.header_for("example.com", "/").is_some());
        assert!(jar.header_for("sub.example.com", "/").is_none());
        assert!(jar.header_for("evil-example.com", "/").is_none());
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.com").len(), 0);
        // host-only should not be visible on parent or unrelated
        assert!(jar.header_for("other.com", "/").is_none());
    }

    #[test]
    fn valid_example_com_matches_subdomain_not_evil() {
        let mut jar = CookieJar::new();
        jar.store_raw("a", "1", ".example.com", None);
        // exact host matches
        assert_eq!(jar.header_for("example.com", "/"), Some("a=1".to_string()));
        // subdomains match
        assert_eq!(
            jar.header_for("sub.example.com", "/"),
            Some("a=1".to_string())
        );
        assert_eq!(
            jar.header_for("deep.sub.example.com", "/"),
            Some("a=1".to_string())
        );
        // dot-boundary prevents evil-example.com
        assert!(jar.header_for("evil-example.com", "/").is_none());
        assert!(jar.header_for("evil.com", "/").is_none());
        // snapshot similarly
        assert_eq!(jar.snapshot_for("sub.example.com").len(), 1);
        assert_eq!(jar.snapshot_for("evil-example.com").len(), 0);
    }

    #[test]
    fn valid_domain_attribute_matching() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "sub.example.com",
            &[(
                "Set-Cookie".to_string(),
                "a=1; Domain=example.com".to_string(),
            )],
        );
        assert!(jar.header_for("sub.example.com", "/").is_some());
        assert!(jar.header_for("example.com", "/").is_some());
        assert!(jar.header_for("other.sub.example.com", "/").is_some());
        assert!(jar.header_for("evil-example.com", "/").is_none());
    }

    #[test]
    fn unrelated_hosts_no_leak() {
        let mut jar = CookieJar::new();
        jar.store_raw("a", "1", ".example.com", None);
        assert!(jar.header_for("other.com", "/").is_none());
        assert!(jar.header_for("example.org", "/").is_none());
        assert!(jar.header_for("example.com.evil.com", "/").is_none());
        assert_eq!(jar.snapshot_for("other.com").len(), 0);
        assert_eq!(jar.snapshot_for("example.org").len(), 0);
    }

    #[test]
    fn malformed_domains_rejected() {
        let mut jar = CookieJar::new();
        // empty label
        jar.store_raw("a", "1", "example..com", None);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // leading hyphen
        jar.store_raw("b", "1", "-example.com", None);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // trailing hyphen
        jar.store_raw("c", "1", "example-.com", None);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // empty
        jar.store_raw("d", "1", "", None);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // control char
        jar.store_raw("e", "1", "example.com\u{00}", None);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // underscore invalid label
        jar.store_raw("f", "1", "exa_mple.com", None);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // double leading dot -> empty label after stripping one
        jar.store_raw("g", "1", "..example.com", None);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);

        // via store_from_headers malformed should fallback to host-only
        let mut jar2 = CookieJar::new();
        jar2.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "a=1; Domain=example..com".to_string(),
            )],
        );
        // fallback host-only: only example.com visible
        assert_eq!(jar2.snapshot_for("example.com").len(), 1);
        assert_eq!(jar2.snapshot_for("sub.example.com").len(), 0);
    }

    #[test]
    fn raw_public_suffix_rejected() {
        let mut jar = CookieJar::new();
        jar.store_raw("a", "1", "com", None);
        assert_eq!(jar.snapshot_for("com").len(), 0);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        jar.store_raw("a", "1", ".com", None);
        assert_eq!(jar.snapshot_for("com").len(), 0);
        jar.store_raw("a", "1", "co.uk", None);
        assert_eq!(jar.snapshot_for("co.uk").len(), 0);
        jar.store_raw("a", "1", ".co.uk", None);
        assert_eq!(jar.snapshot_for("co.uk").len(), 0);
        jar.store_raw("a", "1", ".example.com.", None); // trailing dot should still be valid
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        // but pure public suffix with trailing dot rejected
        jar = CookieJar::new();
        jar.store_raw("a", "1", "com.", None);
        assert_eq!(jar.snapshot_for("com").len(), 0);
    }

    #[test]
    fn case_insensitivity_and_trailing_dot() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "Example.COM",
            &[(
                "Set-Cookie".to_string(),
                "a=1; Domain=EXAMPLE.COM".to_string(),
            )],
        );
        // normalized to lower case
        assert!(jar.header_for("example.com", "/").is_some());
        assert!(jar.header_for("EXAMPLE.COM", "/").is_some());
        assert!(jar.header_for("sub.example.com", "/").is_some());
        // trailing root dot stripped
        let mut jar2 = CookieJar::new();
        jar2.store_raw("a", "1", ".Example.COM.", None);
        assert!(jar2.header_for("example.com", "/").is_some());
        assert!(jar2.header_for("sub.example.com.", "/").is_some());
    }

    #[test]
    fn control_chars_in_domain_rejected() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "a=1; Domain=exa\r\nmple.com".to_string(),
            )],
        );
        // should fallback to host-only
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.com").len(), 0);
    }
}
