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
    /// Set only when a Secure attribute was present AND the
    /// cookie was received over HTTPS. See RFC 6265 \u00a74.1.2.5
    /// and \u00a78.5: a Secure cookie must never travel over a
    /// plain-HTTP channel, in either direction.
    secure: bool,
    /// HttpOnly has no effect on a non-browser client (we have no
    /// script context): it is carried for snapshot/vault
    /// round-trip fidelity only.
    http_only: bool,
    /// "strict" | "lax" | "none", lowercase. SameSite is not
    /// enforced at attach time (our fetch is its own top-level
    /// context, so a direct GET is always a top-level navigation),
    /// it is stored for round-trip fidelity.
    same_site: String,
}

#[derive(Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
    /// Domains currently fed from the session vault (the last
    /// rewrite). A vault rewrite DROPS only these: warm/solve
    /// cookies from a fetch pipeline are never collateral.
    vault_domains: std::collections::HashSet<String>,
}

/// Shared RFC 1035 label validation for hosts and domains:
/// non-empty overall (<=253 bytes), labels 1-63 bytes, no
/// leading/trailing hyphen, lower-case alnum + hyphen only.
fn valid_host_labels(s: &str) -> Option<()> {
    if s.is_empty() || s.len() > 253 {
        return None;
    }
    for label in s.split('.') {
        if label.is_empty() || label.len() > 63 {
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
    Some(())
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
    valid_host_labels(&s)?;
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
    valid_host_labels(&s)?;
    Some(s)
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the vault-sourced subset: drop everything from
    /// domains previously fed by the vault, then store the new
    /// set. Warm pipeline cookies (solves, clearances) from other
    /// domains survive untouched.
    pub fn reset(&mut self, cookies: &[CookieRecord]) {
        // Drop (a) prior vault domains and (b) the incoming
        // domains, so a logout (an absent domain) really clears.
        let mut touched: std::collections::HashSet<String> = self.vault_domains.clone();
        for c in cookies {
            if let Some(d) = normalize_domain(&c.domain) {
                touched.insert(d);
            }
        }
        self.cookies
            .retain(|c| !touched.contains(&normalize_domain(&c.domain).unwrap_or_default()));
        self.vault_domains.clear();
        for c in cookies {
            self.store_raw(c);
            if let Some(d) = normalize_domain(&c.domain) {
                self.vault_domains.insert(d);
            }
        }
    }

    /// Store all Set-Cookie headers from a response for `host`.
    /// `is_https` must reflect the scheme the response arrived
    /// over: Secure cookies received over plain HTTP are dropped
    /// (RFC 6265 §4.1.2.5), and the `__Secure-` / `__Host-`
    /// prefix rules are enforced here.
    pub fn store_from_headers(&mut self, host: &str, headers: &[(String, String)], is_https: bool) {
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
            let mut secure = false;
            let mut http_only = false;
            let mut same_site = "lax".to_string();
            let mut domain_attr_seen = false;
            for attr in parts {
                let attr = attr.trim();
                if let Some((k, val)) = attr.split_once('=') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "domain" => {
                            domain_attr_seen = true;
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
                            // host or a parent of it : otherwise any
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
                        "expires" => {
                            // RFC 6265 5.2.1: a non-parseable Expires is
                            // IGNORED; a past date expires the cookie.
                            // Max-Age takes precedence (5.4): only fill
                            // in from Expires when Max-Age said nothing.
                            if !expired
                                && expires_at.is_none()
                                && let Some(unix) = parse_http_date(val.trim())
                            {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0);
                                if unix <= now {
                                    expired = true;
                                } else {
                                    expires_at = Some(unix);
                                }
                            }
                        }
                        "max-age" => {
                            // RFC 6265 5.2.2: an unparseable Max-Age is
                            // IGNORED (the cookie stays a session
                            // cookie); only a parsed value <= 0 expires.
                            // Was: unwrap_or(1) turned any parse miss
                            // into a 1-second cookie.
                            match val.trim().parse::<i64>() {
                                Ok(secs) if secs <= 0 => expired = true,
                                Ok(secs) => {
                                    expires_at = Some(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.as_secs())
                                            .unwrap_or(0)
                                            + secs as u64,
                                    );
                                }
                                Err(_) => {}
                            }
                        }
                        "samesite" => {
                            same_site = match val.trim().to_ascii_lowercase().as_str() {
                                "strict" => "strict".to_string(),
                                "lax" => "lax".to_string(),
                                "none" => "none".to_string(),
                                _ => "lax".to_string(),
                            };
                        }
                        _ => {}
                    }
                } else {
                    // Bare-flag attributes: Secure, HttpOnly.
                    match attr.to_ascii_lowercase().as_str() {
                        "secure" => secure = true,
                        "httponly" | "http_only" => http_only = true,
                        _ => {}
                    }
                }
            }
            // Attribute consistency rules (RFC 6265bis + prefix
            // semantics). Violations reject the cookie outright:
            // a setter asking for two contradictory things gets
            // neither, and a Secure cookie can never enter the jar
            // over plain HTTP.
            if secure && !is_https {
                continue;
            }
            if same_site == "none" && !secure {
                continue;
            }
            if name.starts_with("__Secure-") && !secure {
                continue;
            }
            if name.starts_with("__Host-") && (!secure || domain_attr_seen || path != "/") {
                continue;
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
                    secure,
                    http_only,
                    same_site,
                });
            }
        }
        self.purge_expired();
    }

    /// Inject a cookie harvested out-of-band (DonGhost
    /// clearance handoff) into the jar. The record carries the
    /// flags the browser saw at set time; a secure harvest must
    /// never replay over plain HTTP, and prefix rules are
    /// enforced on this ingress too.
    pub fn store_raw(&mut self, rec: &CookieRecord) {
        // Same control-character rejection as store_from_headers:
        // CDP-harvested values must never split the Cookie header.
        if rec.name.contains(['\r', '\n', '\0']) || rec.value.contains(['\r', '\n', '\0']) {
            return;
        }
        // Preserve leading-dot subdomain semantics
        let is_subdomain = rec.domain.trim().starts_with('.');
        let Some(normalized) = normalize_domain(&rec.domain) else {
            return;
        };
        // Reject invalid/public-suffix domains
        if psl::domain(normalized.as_bytes()).is_none() {
            return;
        }
        let host_only = !is_subdomain;
        let path = if rec.path.is_empty() {
            "/".to_string()
        } else {
            rec.path.clone()
        };
        // Prefix semantics, mirrored from store_from_headers: the
        // harvest path is a second ingress into the same jar and
        // must not be a route around the rules.
        if rec.name.starts_with("__Secure-") && !rec.secure {
            return;
        }
        if rec.name.starts_with("__Host-") && (!rec.secure || !host_only || path != "/") {
            return;
        }
        let same_site = match rec.same_site.to_ascii_lowercase().as_str() {
            "strict" => "strict",
            "lax" => "lax",
            "none" => "none",
            _ => "lax",
        };
        if same_site == "none" && !rec.secure {
            return;
        }
        self.cookies
            .retain(|c| !(c.name == rec.name && c.domain == normalized && c.path == path));
        self.cookies.push(Cookie {
            name: rec.name.clone(),
            value: rec.value.clone(),
            domain: normalized,
            path,
            host_only,
            expires_at: rec.expires_at,
            secure: rec.secure,
            http_only: rec.http_only,
            same_site: same_site.to_string(),
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
                // Carry the real path: a hard-coded "/" widens
                // path-scoped cookies on the export/import cycle
                // (snapshot -> vault replant -> store_raw), letting a
                // cookie scoped to /secret leak onto the whole domain.
                path: c.path.clone(),
                expires_at: c.expires_at,
                secure: c.secure,
                http_only: c.http_only,
                same_site: c.same_site.clone(),
            })
            .collect()
    }

    /// Whole-jar export (browser cookie-store view), expired
    /// cookies dropped. The tier-1 vault flush (v4 phase 1.4)
    /// persists this so a returning agent replays like a returning
    /// browser device instead of a fresh jar on every process.
    pub fn snapshot_all(&self) -> Vec<CookieRecord> {
        let now = now_secs();
        self.cookies
            .iter()
            .filter(|c| c.expires_at.is_none_or(|e| e > now))
            .map(|c| CookieRecord {
                name: c.name.clone(),
                value: c.value.clone(),
                domain: c.domain.clone(),
                path: c.path.clone(),
                expires_at: c.expires_at,
                secure: c.secure,
                http_only: c.http_only,
                same_site: c.same_site.clone(),
            })
            .collect()
    }

    /// Cookie header value for a request to `host` + `path` over
    /// a channel of the given scheme, if any match. `is_https`
    /// gates the Secure set: a Secure cookie is attached only on
    /// a secure channel.
    pub fn header_for(&self, host: &str, path: &str, is_https: bool) -> Option<String> {
        let normalized_host = normalize_host(host)?;
        let now = now_secs();
        let mut pairs: Vec<&Cookie> = Vec::new();
        for c in &self.cookies {
            // Session cookies (no expiry) always match; expired
            // cookies must never be replayed.
            if c.expires_at.is_some_and(|e| e <= now) {
                continue;
            }
            // Secure cookies never travel over plain HTTP.
            if c.secure && !is_https {
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

/// RFC 7231 IMF-fixdate, plus the two obsolete forms the RFC tells
/// recipients to accept (RFC 850, asctime). Returns unix seconds.
/// Used for the cookie `Expires=` attribute (E14: the date form was
/// previously not parsed at all, so date-expired cookies lived as
/// session cookies and got exported to the vault).
fn parse_http_date(s: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let month_of = |m: &str| {
        MONTHS
            .iter()
            .position(|x| m.len() >= 3 && m[..3].eq_ignore_ascii_case(x))
    };
    let secs_of = |y: i64, mo: usize, d: i64, hh: i64, mm: i64, ss: i64| {
        if !(1..=12).contains(&(mo as i64 + 1)) || !(1..=31).contains(&d) {
            return None;
        }
        // Howard Hinnant's days_from_civil.
        let mp: i64 = (mo as i64 + 10) % 12;
        let y = if mo >= 2 { y } else { y - 1 };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Some(((days * 86_400) + hh * 3600 + mm * 60 + ss).max(0) as u64)
    };
    let s = s.trim();
    // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT
    if let Some(rest) = s.split_once(", ") {
        let parts: Vec<&str> = rest.1.split_ascii_whitespace().collect();
        if parts.len() >= 4 {
            let day: i64 = parts[0].parse().ok()?;
            let mon = month_of(parts[1])?;
            let year: i64 = parts[2].parse().ok()?;
            let t: Vec<&str> = parts[3].split(':').collect();
            if t.len() == 3
                && let (Ok(hh), Ok(mm), Ok(ss)) = (
                    t[0].parse::<i64>(),
                    t[1].parse::<i64>(),
                    t[2].parse::<i64>(),
                )
            {
                return secs_of(year, mon, day, hh, mm, ss);
            }
        }
        // RFC 850: Sunday, 06-Nov-94 08:49:37 GMT
        let dmy: Vec<&str> = rest.1.split_ascii_whitespace().collect();
        if !dmy.is_empty() {
            let seg: Vec<&str> = dmy[0].split('-').collect();
            if seg.len() == 3 {
                let day: i64 = seg[0].parse().ok()?;
                let mon = month_of(seg[1])?;
                let yy: i64 = seg[2].parse().ok()?;
                let year = if yy < 100 {
                    if yy < 70 { 2000 + yy } else { 1900 + yy }
                } else {
                    yy
                };
                let t: Vec<&str> = dmy.get(1)?.split(':').collect();
                if t.len() == 3
                    && let (Ok(hh), Ok(mm), Ok(ss)) = (
                        t[0].parse::<i64>(),
                        t[1].parse::<i64>(),
                        t[2].parse::<i64>(),
                    )
                {
                    return secs_of(year, mon, day, hh, mm, ss);
                }
            }
        }
        return None;
    }
    // asctime: Sun Nov  6 08:49:37 1994
    let parts: Vec<&str> = s.split_ascii_whitespace().collect();
    if parts.len() == 5 {
        let mon = month_of(parts[1])?;
        let day: i64 = parts[2].parse().ok()?;
        let t: Vec<&str> = parts[3].split(':').collect();
        if t.len() == 3
            && let (Ok(hh), Ok(mm), Ok(ss)) = (
                t[0].parse::<i64>(),
                t[1].parse::<i64>(),
                t[2].parse::<i64>(),
            )
        {
            let year: i64 = parts[4].parse().ok()?;
            return secs_of(year, mon, day, hh, mm, ss);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(domain: &str, name: &str) -> CookieRecord {
        CookieRecord {
            domain: domain.into(),
            path: "/".into(),
            name: name.into(),
            value: "v".into(),
            expires_at: None,
            http_only: false,
            secure: false,
            same_site: "Lax".into(),
        }
    }

    /// Vault rewrites touch ONLY vault-sourced domains: warm solve
    /// cookies from the fetch pipeline survive a login resync.
    #[test]
    fn reset_preserves_non_vault_domains_and_applies_logout() {
        let mut jar = CookieJar::new();
        jar.store_raw(&rec("solve-host.test", "clearance"));
        jar.reset(&[rec(".x.com", "AUTH")]);
        // Both present after login.
        assert!(jar.snapshot_for("x.com").iter().any(|c| c.name == "AUTH"));
        assert!(
            jar.snapshot_for("solve-host.test")
                .iter()
                .any(|c| c.name == "clearance")
        );

        // Logout: the vault set empties, the warm cookie survives.
        jar.reset(&[]);
        assert!(jar.snapshot_for("x.com").is_empty());
        assert!(
            jar.snapshot_for("solve-host.test")
                .iter()
                .any(|c| c.name == "clearance")
        );

        // Subdomain cleanup: a vault rewrite for the apex also drops
        // previously vaulted subdomain cookies of the same site.
        let mut jar2 = CookieJar::new();
        jar2.reset(&[rec("login.x.com", "host_only")]);
        jar2.reset(&[]);
        assert!(jar2.snapshot_for("login.x.com").is_empty());
    }

    #[test]
    fn domain_com_not_shared() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[("Set-Cookie".to_string(), "a=1; Domain=com".to_string())],
            false,
        );
        // Public suffix should be rejected -> fallback to host-only
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.com").len(), 0);
        assert_eq!(jar.snapshot_for("evil.com").len(), 0);
        assert!(jar.header_for("example.com", "/", false).is_some());
        assert!(jar.header_for("sub.example.com", "/", false).is_none());
        assert!(jar.header_for("other.com", "/", false).is_none());
    }

    #[test]
    fn domain_co_uk_not_shared() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.co.uk",
            &[("Set-Cookie".to_string(), "a=1; Domain=co.uk".to_string())],
            false,
        );
        assert_eq!(jar.snapshot_for("example.co.uk").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.co.uk").len(), 0);
        assert_eq!(jar.snapshot_for("evil.co.uk").len(), 0);
        assert!(jar.header_for("example.co.uk", "/", false).is_some());
        assert!(jar.header_for("sub.example.co.uk", "/", false).is_none());
    }

    #[test]
    fn host_only_exact_match() {
        let mut jar = CookieJar::new();
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert!(jar.header_for("example.com", "/", false).is_some());
        assert!(jar.header_for("sub.example.com", "/", false).is_none());
        assert!(jar.header_for("evil-example.com", "/", false).is_none());
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.com").len(), 0);
        // host-only should not be visible on parent or unrelated
        assert!(jar.header_for("other.com", "/", false).is_none());
    }

    #[test]
    fn valid_example_com_matches_subdomain_not_evil() {
        let mut jar = CookieJar::new();
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: ".example.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        // exact host matches
        assert_eq!(
            jar.header_for("example.com", "/", false),
            Some("a=1".to_string())
        );
        // subdomains match
        assert_eq!(
            jar.header_for("sub.example.com", "/", false),
            Some("a=1".to_string())
        );
        assert_eq!(
            jar.header_for("deep.sub.example.com", "/", false),
            Some("a=1".to_string())
        );
        // dot-boundary prevents evil-example.com
        assert!(jar.header_for("evil-example.com", "/", false).is_none());
        assert!(jar.header_for("evil.com", "/", false).is_none());
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
            false,
        );
        assert!(jar.header_for("sub.example.com", "/", false).is_some());
        assert!(jar.header_for("example.com", "/", false).is_some());
        assert!(
            jar.header_for("other.sub.example.com", "/", false)
                .is_some()
        );
        assert!(jar.header_for("evil-example.com", "/", false).is_none());
    }

    #[test]
    fn unrelated_hosts_no_leak() {
        let mut jar = CookieJar::new();
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: ".example.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert!(jar.header_for("other.com", "/", false).is_none());
        assert!(jar.header_for("example.org", "/", false).is_none());
        assert!(jar.header_for("example.com.evil.com", "/", false).is_none());
        assert_eq!(jar.snapshot_for("other.com").len(), 0);
        assert_eq!(jar.snapshot_for("example.org").len(), 0);
    }

    #[test]
    fn malformed_domains_rejected() {
        let mut jar = CookieJar::new();
        // empty label
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: "example..com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // leading hyphen
        jar.store_raw(&CookieRecord {
            name: "b".to_string(),
            value: "1".to_string(),
            domain: "-example.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // trailing hyphen
        jar.store_raw(&CookieRecord {
            name: "c".to_string(),
            value: "1".to_string(),
            domain: "example-.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // empty
        jar.store_raw(&CookieRecord {
            name: "d".to_string(),
            value: "1".to_string(),
            domain: "".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // control char
        jar.store_raw(&CookieRecord {
            name: "e".to_string(),
            value: "1".to_string(),
            domain: "example.com\u{00}".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // underscore invalid label
        jar.store_raw(&CookieRecord {
            name: "f".to_string(),
            value: "1".to_string(),
            domain: "exa_mple.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // double leading dot -> empty label after stripping one
        jar.store_raw(&CookieRecord {
            name: "g".to_string(),
            value: "1".to_string(),
            domain: "..example.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("example.com").len(), 0);

        // via store_from_headers malformed should fallback to host-only
        let mut jar2 = CookieJar::new();
        jar2.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "a=1; Domain=example..com".to_string(),
            )],
            false,
        );
        // fallback host-only: only example.com visible
        assert_eq!(jar2.snapshot_for("example.com").len(), 1);
        assert_eq!(jar2.snapshot_for("sub.example.com").len(), 0);
    }

    #[test]
    fn raw_public_suffix_rejected() {
        let mut jar = CookieJar::new();
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: "com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("com").len(), 0);
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: ".com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("com").len(), 0);
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: "co.uk".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("co.uk").len(), 0);
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: ".co.uk".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.snapshot_for("co.uk").len(), 0);
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: ".example.com.".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        }); // trailing dot should still be valid
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        // but pure public suffix with trailing dot rejected
        jar = CookieJar::new();
        jar.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: "com.".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
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
            false,
        );
        // normalized to lower case
        assert!(jar.header_for("example.com", "/", false).is_some());
        assert!(jar.header_for("EXAMPLE.COM", "/", false).is_some());
        assert!(jar.header_for("sub.example.com", "/", false).is_some());
        // trailing root dot stripped
        let mut jar2 = CookieJar::new();
        jar2.store_raw(&CookieRecord {
            name: "a".to_string(),
            value: "1".to_string(),
            domain: ".Example.COM.".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert!(jar2.header_for("example.com", "/", false).is_some());
        assert!(jar2.header_for("sub.example.com.", "/", false).is_some());
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
            false,
        );
        // should fallback to host-only
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
        assert_eq!(jar.snapshot_for("sub.example.com").len(), 0);
    }

    // -- Secure attribute semantics (GHSA draft, mnaza) -----------

    #[test]
    fn secure_cookie_never_replays_over_plain_http() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "sess=SECRET; Secure; Path=/".to_string(),
            )],
            true,
        );
        assert_eq!(jar.header_for("example.com", "/", false), None);
        assert_eq!(
            jar.header_for("example.com", "/", true),
            Some("sess=SECRET".to_string())
        );
    }

    #[test]
    fn nonsecure_cookie_replays_over_both_schemes() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[("Set-Cookie".to_string(), "a=1".to_string())],
            true,
        );
        assert!(jar.header_for("example.com", "/", false).is_some());
        assert!(jar.header_for("example.com", "/", true).is_some());
    }

    #[test]
    fn secure_cookie_from_plain_http_response_is_dropped() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "sess=SECRET; Secure; Path=/".to_string(),
            )],
            false,
        );
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        assert_eq!(jar.header_for("example.com", "/", false), None);
    }

    #[test]
    fn imported_secure_cookie_respects_the_flag() {
        let mut jar = CookieJar::new();
        jar.store_raw(&CookieRecord {
            name: "sess".to_string(),
            value: "SECRET".to_string(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            expires_at: None,
            secure: true,
            http_only: false,
            same_site: "Lax".to_string(),
        });
        assert_eq!(jar.header_for("example.com", "/", false), None);
        assert_eq!(
            jar.header_for("example.com", "/", true),
            Some("sess=SECRET".to_string())
        );
        // round-trips into the snapshot with the real flags
        let snap = jar.snapshot_for("example.com");
        assert_eq!(snap.len(), 1);
        assert!(snap[0].secure);
        assert!(!snap[0].http_only);
    }

    #[test]
    fn httponly_flag_roundtrips_but_never_blocks_replay() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "a=1; HttpOnly; Path=/".to_string(),
            )],
            true,
        );
        let snap = jar.snapshot_for("example.com");
        assert!(snap[0].http_only);
        assert!(jar.header_for("example.com", "/", true).is_some());
    }

    #[test]
    fn secure_prefix_cookie_requires_secure_attribute() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "__Secure-sess=SECRET; Path=/".to_string(),
            )],
            true,
        );
        assert_eq!(jar.snapshot_for("example.com").len(), 0);

        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "__Secure-sess=SECRET; Secure; Path=/".to_string(),
            )],
            true,
        );
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
    }

    #[test]
    fn host_prefix_cookie_requires_secure_host_only_and_root_path() {
        let mut jar = CookieJar::new();
        // No Secure attribute: rejected.
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "__Host-sess=SECRET; Path=/".to_string(),
            )],
            true,
        );
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // Domain attribute: rejected even with Secure.
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "__Host-sess=SECRET; Secure; Domain=example.com; Path=/".to_string(),
            )],
            true,
        );
        assert_eq!(jar.snapshot_for("example.com").len(), 0);
        // Correct form: accepted.
        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "__Host-sess=SECRET; Secure; Path=/".to_string(),
            )],
            true,
        );
        assert_eq!(jar.snapshot_for("example.com").len(), 1);
    }

    #[test]
    fn samesite_none_without_secure_is_dropped() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.com",
            &[("Set-Cookie".to_string(), "a=1; SameSite=None".to_string())],
            true,
        );
        assert_eq!(jar.snapshot_for("example.com").len(), 0);

        jar.store_from_headers(
            "example.com",
            &[(
                "Set-Cookie".to_string(),
                "a=1; SameSite=None; Secure".to_string(),
            )],
            true,
        );
        let snap = jar.snapshot_for("example.com");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].same_site, "none");
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    fn parse_http_date_imf_fixdate() {
        let secs = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(secs, 784111777, "canonical RFC 7231 example");
        let leap = parse_http_date("Wed, 29 Feb 2023 00:00:00 GMT");
        assert!(
            leap.is_some(),
            "non-leap Feb 29 still parses (validates range)"
        );
    }

    #[test]
    fn parse_http_date_rfc850_and_asctime() {
        // Sunday, 06-Nov-94 08:49:37 GMT == the same instant.
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT")
        );
        // asctime: Sun Nov  6 08:49:37 1994
        assert_eq!(
            parse_http_date("Sun Nov  6 08:49:37 1994"),
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT")
        );
    }

    #[test]
    fn parse_http_date_rejects_garbage() {
        assert!(parse_http_date("not a date").is_none());
        assert!(parse_http_date("").is_none());
        assert!(parse_http_date("Sun, 99 Xxx 1994 08:49:37 GMT").is_none());
    }

    #[test]
    fn store_from_headers_expires_date_form_is_honored() {
        let mut jar = CookieJar::new();
        let host = "example.org";
        let future = "Wed, 01 Jan 2031 00:00:00 GMT";
        jar.store_from_headers(
            host,
            &[(
                "set-cookie".to_string(),
                format!("sid=1; Path=/p; Expires={future}"),
            )],
            true,
        );
        let snap = jar.snapshot_for("example.org");
        assert!(
            snap.iter()
                .any(|c| c.name == "sid" && c.path == "/p" && c.expires_at.is_some()),
            "date-form expiry parsed and carried: {snap:?}"
        );
    }

    #[test]
    fn store_from_headers_max_age_wins_over_expires() {
        let mut jar = CookieJar::new();
        let host = "example.org";
        jar.store_from_headers(
            host,
            &[(
                "set-cookie".to_string(),
                "sid=2; Max-Age=3600; Expires=Thu, 01 Jan 1970 00:00:00 GMT".to_string(),
            )],
            true,
        );
        let snap = jar.snapshot_for("example.org");
        let c = snap
            .iter()
            .find(|c| c.name == "sid")
            .expect("cookie stored");
        assert!(c.expires_at.is_some(), "Max-Age kept the cookie alive");
        assert!(
            c.expires_at.unwrap() > now_secs(),
            "Expires in 1970 must not win"
        );
    }

    #[test]
    fn store_from_headers_unparseable_max_age_stays_session_cookie() {
        let mut jar = CookieJar::new();
        let host = "example.org";
        jar.store_from_headers(
            host,
            &[("set-cookie".to_string(), "sid=3; Max-Age=abc".to_string())],
            true,
        );
        let snap = jar.snapshot_for("example.org");
        let c = snap
            .iter()
            .find(|c| c.name == "sid")
            .expect("cookie stored");
        assert!(
            c.expires_at.is_none(),
            "unparseable Max-Age = session cookie (RFC 6265)"
        );
    }

    #[test]
    fn store_from_headers_past_expires_expires_immediately() {
        let mut jar = CookieJar::new();
        let host = "example.org";
        jar.store_from_headers(
            host,
            &[(
                "set-cookie".to_string(),
                "sid=4; Expires=Thu, 01 Jan 1970 00:00:00 GMT".to_string(),
            )],
            true,
        );
        let snap = jar.snapshot_for("example.org");
        assert!(
            !snap.iter().any(|c| c.name == "sid"),
            "past-dated Expires must not leave a live cookie"
        );
    }

    #[test]
    fn snapshot_for_carries_real_paths() {
        let mut jar = CookieJar::new();
        jar.store_from_headers(
            "example.org",
            &[("set-cookie".to_string(), "k=v; Path=/secret".to_string())],
            true,
        );
        let snap = jar.snapshot_for("example.org");
        let c = snap.iter().find(|c| c.name == "k").expect("cookie");
        assert_eq!(c.path, "/secret", "export must keep the real path (B6)");
    }
}
