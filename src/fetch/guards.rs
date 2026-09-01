//! Fetch guards: SSRF prevention and binary content detection.
//!
//! These run BEFORE any network or extraction step so the caller
//! gets a clean, structured error instead of raw bytes or a
//! connection to a private address.

use std::net::IpAddr;

/// True if the URL's host is a private/loopback/link-local
/// address that must never be fetched (SSRF guard).
///
/// Handles literal IPs and well-known localhost names. This
/// is a synchronous helper that only handles literal/obvious
/// names : hostname DNS safety is enforced by the async
/// `ensure_url_safe` which resolves hostnames and rejects
/// private addresses.
pub fn is_ssrf_host(host: &str) -> bool {
    // url::Url::host_str() keeps brackets on IPv6 literals :
    // strip them so the IP parser actually sees an IP.
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    // Literal IP?
    if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        return is_private_ip(&ip);
    }
    // Well-known localhost names.
    let h = host.to_lowercase();
    h == "localhost"
        || h == "localhost."
        || h.ends_with(".localhost")
        || h == "0.0.0.0"
        || h == "[::1]"
        || h == "::1"
}

/// IP-level SSRF check for post-resolution validation.
/// A hostname that resolves to a private address is just as
/// dangerous as a literal one (DNS pinning closes the
/// hostname/rebinding bypass).
pub fn is_ssrf_ip(ip: &IpAddr) -> bool {
    is_private_ip(ip)
}

/// IP-level SSRF check for DNS-resolved addresses (post-resolution
/// pinning tier): same strictness as `is_ssrf_ip` EXCEPT the RFC
/// 2544 benchmarking block 198.18.0.0/15. Fake-ip TUN networks
/// (mihomo/Clash/Surge/etc.) map every hostname into that block and
/// route the actual dial through the TUN device; treating it as
/// private bricks the tool for those users while protecting nothing:
/// the block is an IETF-reserved no-service range, not a reachable
/// private network. Every real SSRF surface (RFC 1918, loopback,
/// link-local, CGNAT, ULA, documentation ranges) stays blocked at
/// the resolved tier. Literal 198.18.x in a URL stays blocked via
/// `is_ssrf_ip`: a literal has no legitimate destination.
pub fn is_ssrf_resolved_ip(ip: &IpAddr) -> bool {
    if let IpAddr::V4(v4) = ip {
        let o = v4.octets();
        if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
            return false;
        }
    }
    is_private_ip(ip)
}

/// Escape hatch for deliberate private egress (CLI power users,
/// local services): DONSETCH_ALLOW_PRIVATE_EGRESS must be explicitly
/// true to disable the SSRF guard chain end to end. Default off.
pub(crate) fn private_egress_allowed() -> bool {
    crate::config::env_flag("DONSETCH_ALLOW_PRIVATE_EGRESS")
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || (v4.octets()[0] == 198 && (v4.octets()[1] == 18 || v4.octets()[1] == 19)) // 198.18.0.0/15 benchmarking
                || (v4.octets()[0] >= 240) // 240.0.0.0/4 reserved
                || (v4.octets()[0] == 0) // 0.0.0.0/8 (software)
                // Carrier-grade NAT: 100.64.0.0/10
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
                // 192.88.99.0/24 (6to4 relay anycast) : deprecated/reserved
                || (v4.octets()[0] == 192 && v4.octets()[1] == 88 && v4.octets()[2] == 99)
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) is the v4 address :
            // check it as v4 or it slips past every v6 rule.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(&IpAddr::V4(v4));
            }
            // IPv4-compat ::a.b.c.d (deprecated) also maps.
            if let Some(v4) = v6.to_ipv4() {
                // to_ipv4 returns Some for ::ffff: and :: forms;
                // if we already handled mapped, this catches compat.
                // Only treat as v4 if the v6 is in ::/96 compat range.
                let segs = v6.segments();
                if segs[0] == 0 && segs[1] == 0 && segs[2] == 0 && segs[3] == 0 && segs[4] == 0 {
                    return is_private_ip(&IpAddr::V4(v4));
                }
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Link-local: fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // Unique local: fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Documentation: 2001:db8::/32
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
                // Benchmarking / reserved etc.: treat 0100::/64? Use generic reserved check via multicast+unspecified already.
                // Discard prefix 100::/64 is also reserved.
                || (v6.segments()[0] == 0x0100)
        }
    }
}

/// Centralized URL safety check (synchronous part).
///
/// Validates:
/// - scheme is http or https only
/// - no credentials in URL (username/password)
/// - host present and not SSRF (literal IP ranges, localhost names)
/// - does NOT do DNS resolution (use `ensure_url_safe` for that)
///
/// Returns the parsed Url on success, or a FetchError that the
/// caller should surface directly. This is the single sync gate
/// used by both fetch and browser tiers.
pub fn validate_url_basic(url_str: &str) -> Result<url::Url, crate::error::FetchError> {
    validate_url_basic_with_policy(url_str, private_egress_allowed())
}

fn validate_url_basic_with_policy(
    url_str: &str,
    allow_private_egress: bool,
) -> Result<url::Url, crate::error::FetchError> {
    let url = url::Url::parse(url_str)
        .map_err(|_| crate::error::FetchError::InvalidUrl(url_str.into()))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(crate::error::FetchError::Http(format!(
            "blocked: scheme {scheme} not allowed : only http/https"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(crate::error::FetchError::Http(
            "blocked: URL contains credentials : SSRF guard".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| crate::error::FetchError::InvalidUrl(url_str.into()))?;
    if is_ssrf_host(host) && !allow_private_egress {
        return Err(crate::error::FetchError::Http(format!(
            "blocked: {host} is a private/loopback address : SSRF guard (set DONSETCH_ALLOW_PRIVATE_EGRESS to override)"
        )));
    }
    // Also check host_str for bracketed IPv6 that Url keeps brackets on? is_ssrf_host handles it.
    Ok(url)
}

/// Async URL safety check that includes DNS resolution.
///
/// Does everything `validate_url_basic` does, plus:
/// - resolves the hostname via tokio::net::lookup_host
/// - rejects if ANY resolved address is SSRF (private/loopback/etc.)
/// - **fail-closed** on DNS resolution failure/timeout for
///   network/browser navigation (the `ensure_url_safe` path).
///   The synchronous `validate_url_basic` is the explicitly
///   justified offline-only path (no DNS, no network) and is used
///   by the fetch tier's initial literal check and by callers
///   that deliberately defer DNS to the transport layer
///   (`transport::tcp::happy_connect` re-validates at connect).
///
/// # DNS rebinding / TOCTOU limitation
///
/// This is a point-in-time check. DNS can change between this
/// validation and the actual TCP connect (DNS rebinding / TOCTOU).
/// The transport layer (`transport::tcp::happy_connect`) re-validates
/// resolved addresses at connect time and filters private IPs, so
/// an attacker that returns public at validation and private at
/// connect is still blocked at connect. However, without full
/// DNS pinning (reusing the validated IPs for the connect) there
/// is a residual window between the two resolutions. Full pinning
/// is not implemented; this is documented as a residual limitation.
/// Redirects are re-validated per hop via `validate_redirect_url`
/// (sync) and `ensure_url_safe` (async) where applicable.
pub async fn ensure_url_safe(url_str: &str) -> Result<url::Url, crate::error::FetchError> {
    let allow_private_egress = private_egress_allowed();
    let url = validate_url_basic_with_policy(url_str, allow_private_egress)?;
    // Deliberate opt-out: skip the DNS resolution tier entirely.
    if allow_private_egress {
        return Ok(url);
    }
    let host: String = url.host_str().unwrap_or("").to_owned();
    // Literal IP already blocked by validate_url_basic; only hostnames need DNS.
    // Fail closed on resolution errors for browser/network navigation.
    let port = url.port_or_known_default().unwrap_or(443);
    let lookup = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await;
    match lookup {
        Ok(Ok(addrs)) => {
            let mut any = false;
            for addr in addrs {
                any = true;
                if is_ssrf_resolved_ip(&addr.ip()) {
                    return Err(crate::error::FetchError::Http(format!(
                        "blocked: {host} resolves to private/loopback address {} : SSRF guard (set DONSETCH_ALLOW_PRIVATE_EGRESS to override)",
                        addr.ip()
                    )));
                }
            }
            if !any {
                return Err(crate::error::FetchError::Http(format!(
                    "blocked: {host} DNS returned no addresses : fail-closed SSRF guard"
                )));
            }
            Ok(url)
        }
        Ok(Err(e)) => Err(crate::error::FetchError::Http(format!(
            "blocked: DNS resolution failed for {host}: {e} : fail-closed SSRF guard"
        ))),
        Err(_) => Err(crate::error::FetchError::Http(format!(
            "blocked: DNS resolution timeout for {host} : fail-closed SSRF guard"
        ))),
    }
}

/// Validate a redirect target URL string relative to a base URL.
/// Rejects non-http(s) schemes and SSRF hosts. Used per redirect hop.
pub fn validate_redirect_url(
    base: &url::Url,
    location: &str,
) -> Result<url::Url, crate::error::FetchError> {
    let next = base
        .join(location)
        .map_err(|_| crate::error::FetchError::Http(format!("bad redirect target: {location}")))?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(crate::error::FetchError::Http(format!(
            "blocked redirect to non-http(s) scheme: {}",
            next.scheme()
        )));
    }
    // Apply centralized validation to the joined URL so missing hosts
    // and credentials are rejected consistently with direct fetches.
    validate_url_basic(next.as_str())?;
    Ok(next)
}

/// Header values must never carry CR/LF/NUL: a value smuggled
/// from a response (e.g. a cookie) into a request line would
/// split/inject headers on the wire (request splitting).
pub fn valid_header_value(v: &str) -> bool {
    !v.contains('\r') && !v.contains('\n') && !v.contains('\0')
}

/// True if the content-type header indicates binary (non-text)
/// content that should not be passed to the extract pipeline.
pub fn is_binary_content_type(ct: &str) -> bool {
    let ct = ct.to_lowercase();
    let ct = ct.split(';').next().unwrap_or("").trim();
    // Allow text-ish types through.
    if ct.is_empty()
        || ct.starts_with("text/")
        || ct.contains("html")
        || ct.contains("xml")
        || ct.contains("json")
        || ct.contains("javascript")
        || ct.contains("pdf")
        || ct.contains("rss")
        || ct.contains("atom")
        || ct.contains("yaml")
        || ct.contains("csv")
        || ct == "application/x-www-form-urlencoded"
    {
        return false;
    }
    // Everything else under image/video/audio/application is binary.
    ct.starts_with("image/")
        || ct.starts_with("video/")
        || ct.starts_with("audio/")
        || ct.starts_with("font/")
        || ct.starts_with("application/")
            && !ct.contains("json")
            && !ct.contains("xml")
            && !ct.contains("javascript")
            && !ct.contains("pdf")
}

pub fn is_pdf(body: &[u8], content_type: &str) -> bool {
    (body.len() >= 5 && body.starts_with(b"%PDF-")) || content_type.to_lowercase().contains("pdf")
}

pub fn is_binary_body(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    if body.starts_with(b"%PDF-") {
        return false;
    }
    const MAGIC: &[&[u8]] = &[
        b"\x89PNG",
        b"\xff\xd8\xff",
        b"GIF8",
        b"BM",
        b"\x1f\x8b",
        b"PK\x03\x04",
        b"\x7fELF",
        b"\x00\x00\x01\x00",
        b"\x00\x00\x02\x00",
        b"RIFF",
        b"\x00asm",
    ];
    for m in MAGIC {
        if body.starts_with(m) {
            return true;
        }
    }
    let scan = &body[..body.len().min(1024)];
    let nulls = scan.iter().filter(|&&b| b == 0).count();
    nulls > 3 || (nulls > 0 && nulls as f64 / scan.len() as f64 > 0.01)
}

pub fn is_binary(body: &[u8], content_type: &str) -> bool {
    if is_pdf(body, content_type) {
        return false;
    }
    is_binary_content_type(content_type) || is_binary_body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_blocks_loopback_v4() {
        assert!(is_ssrf_host("127.0.0.1"));
        assert!(is_ssrf_host("127.0.1.5"));
        assert!(is_ssrf_host("10.0.0.1"));
        assert!(is_ssrf_host("192.168.1.1"));
        assert!(is_ssrf_host("172.16.0.1"));
        assert!(is_ssrf_host("172.31.255.255"));
        assert!(is_ssrf_host("169.254.1.1"));
        assert!(is_ssrf_host("0.0.0.0"));
    }

    #[test]
    fn ssrf_blocks_loopback_v6() {
        assert!(is_ssrf_host("::1"));
        assert!(is_ssrf_host("fe80::1"));
        assert!(is_ssrf_host("fc00::1"));
        assert!(is_ssrf_host("fd12:3456::1"));
    }

    #[test]
    fn ssrf_blocks_localhost_names() {
        assert!(is_ssrf_host("localhost"));
        assert!(is_ssrf_host("localhost."));
        assert!(is_ssrf_host("myapp.localhost"));
    }

    #[test]
    fn ssrf_allows_public() {
        assert!(!is_ssrf_host("93.184.216.34"));
        assert!(!is_ssrf_host("example.com"));
        assert!(!is_ssrf_host("1.1.1.1"));
        assert!(!is_ssrf_host("8.8.8.8"));
    }

    #[test]
    fn ssrf_carrier_grade_nat() {
        assert!(is_ssrf_host("100.64.0.1"));
        assert!(!is_ssrf_host("100.128.0.1"));
    }

    #[test]
    fn ssrf_blocks_ipv4_mapped_v6() {
        assert!(is_ssrf_host("[::ffff:127.0.0.1]"));
        assert!(is_ssrf_host("[::ffff:169.254.169.254]"));
        assert!(is_ssrf_host("[::ffff:10.0.0.1]"));
        assert!(is_ssrf_host("[fd12:3456::1]"));
        assert!(is_ssrf_host("[fe80::1]"));
    }

    #[test]
    fn ssrf_ip_level_check() {
        use std::net::IpAddr;
        let ip: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(is_ssrf_ip(&ip));
        let ip: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(!is_ssrf_ip(&ip));
    }

    #[test]
    fn header_value_validation() {
        assert!(valid_header_value("plain value; charset=utf-8"));
        assert!(!valid_header_value("a\r\nX-Evil: 1"));
        assert!(!valid_header_value("a\nb"));
        assert!(!valid_header_value("a\0b"));
    }

    #[test]
    fn binary_content_type_detection() {
        assert!(is_binary_content_type("image/png"));
        assert!(is_binary_content_type("image/jpeg"));
        assert!(is_binary_content_type("video/mp4"));
        assert!(is_binary_content_type("audio/mpeg"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(is_binary_content_type("application/zip"));
        assert!(is_binary_content_type("font/woff2"));
    }

    #[test]
    fn text_content_type_allowed() {
        assert!(!is_binary_content_type("text/html"));
        assert!(!is_binary_content_type("text/plain"));
        assert!(!is_binary_content_type("text/html; charset=utf-8"));
        assert!(!is_binary_content_type("application/json"));
        assert!(!is_binary_content_type("application/xml"));
        assert!(!is_binary_content_type("application/pdf"));
        assert!(!is_binary_content_type("application/javascript"));
        assert!(!is_binary_content_type("text/csv"));
        assert!(!is_binary_content_type("application/rss+xml"));
        assert!(!is_binary_content_type(""));
    }

    #[test]
    fn binary_body_null_bytes() {
        assert!(is_binary_body(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a]));
        assert!(is_binary_body(b"hello\x00world\x00\x00\x00"));
        assert!(!is_binary_body(b"hello world"));
        assert!(!is_binary_body(b""));
        assert!(!is_binary_body(b"<html>no nulls here</html>"));
    }

    #[test]
    fn is_binary_combines_both() {
        assert!(is_binary(b"\x00\x00\x00", "text/html"));
        assert!(is_binary(b"fake png", "image/png"));
        assert!(!is_binary(b"hello", "text/plain"));
        assert!(!is_binary(b"<html>", "text/html; charset=utf-8"));
    }

    #[test]
    fn pdf_not_binary_by_magic() {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        pdf.extend_from_slice(&[0x00; 200]);
        pdf.extend_from_slice(b"\n%%EOF\n");
        assert!(
            !is_binary_body(&pdf),
            "PDF body must not be flagged as binary"
        );
        assert!(!is_binary(&pdf, "application/pdf"));
        assert!(is_pdf(&pdf, "application/pdf"));
    }

    #[test]
    fn pdf_not_binary_by_content_type() {
        let body = b"\x00\x00\x00\x00\x00\x00\x00\x00";
        assert!(!is_binary(body, "application/pdf"));
    }

    #[test]
    fn pdf_detected_by_magic_bytes() {
        assert!(is_pdf(b"%PDF-1.7", ""));
        assert!(is_pdf(b"%PDF-1.4", "application/octet-stream"));
        assert!(is_pdf(b"not a pdf", "application/pdf"));
        assert!(!is_pdf(b"<html>", "text/html"));
    }

    // New centralized URL validation tests (task 2 regressions)
    #[test]
    fn validate_url_rejects_credentials() {
        assert!(validate_url_basic("https://user:pass@example.com/").is_err());
        assert!(validate_url_basic("https://user@example.com/").is_err());
        assert!(validate_url_basic("https://example.com/").is_ok());
    }

    #[test]
    fn validate_url_rejects_non_http() {
        assert!(validate_url_basic("file:///etc/passwd").is_err());
        assert!(validate_url_basic("ftp://example.com/file").is_err());
        assert!(validate_url_basic("javascript:alert(1)").is_err());
        assert!(validate_url_basic("data:text/html,hi").is_err());
        assert!(validate_url_basic("http://example.com/").is_ok());
        assert!(validate_url_basic("https://example.com/").is_ok());
    }

    #[test]
    fn validate_url_blocks_localhost() {
        assert!(validate_url_basic("http://localhost/").is_err());
        assert!(validate_url_basic("http://127.0.0.1/").is_err());
        assert!(validate_url_basic("http://[::1]/").is_err());
        assert!(validate_url_basic("http://10.0.0.1/").is_err());
        assert!(validate_url_basic("http://192.168.1.1/").is_err());
    }

    #[test]
    fn validate_url_blocks_ipv4_mapped() {
        assert!(validate_url_basic("http://[::ffff:127.0.0.1]/").is_err());
        assert!(validate_url_basic("http://[::ffff:10.0.0.1]/").is_err());
    }

    #[test]
    fn validate_url_blocks_multicast_and_reserved() {
        assert!(is_ssrf_host("224.0.0.1"));
        assert!(is_ssrf_host("239.255.255.250"));
        assert!(is_ssrf_host("ff02::1"));
        assert!(is_ssrf_host("192.0.2.1"));
        assert!(is_ssrf_host("198.51.100.1"));
        assert!(is_ssrf_host("203.0.113.1"));
        assert!(validate_url_basic("http://224.0.0.1/").is_err());
        assert!(validate_url_basic("http://192.0.2.1/").is_err());
    }

    #[test]
    fn validate_url_allows_public() {
        assert!(validate_url_basic("https://example.com/").is_ok());
        assert!(validate_url_basic("https://93.184.216.34/").is_ok());
        assert!(validate_url_basic("https://8.8.8.8/path?q=1").is_ok());
    }

    #[test]
    fn validate_redirect_blocks_ssrf() {
        let base = url::Url::parse("https://example.com/").unwrap();
        assert!(validate_redirect_url(&base, "http://127.0.0.1/evil").is_err());
        assert!(validate_redirect_url(&base, "http://10.0.0.1/").is_err());
        assert!(validate_redirect_url(&base, "file:///etc/passwd").is_err());
        assert!(validate_redirect_url(&base, "https://example.com/other").is_ok());
        assert!(validate_redirect_url(&base, "/relative").is_ok());
    }

    #[tokio::test]
    async fn ensure_url_safe_blocks_literal() {
        assert!(ensure_url_safe("http://127.0.0.1/").await.is_err());
        assert!(ensure_url_safe("http://[::1]/").await.is_err());
        // Public literal IPs require no DNS and must pass.
        assert!(ensure_url_safe("https://93.184.216.34/").await.is_ok());
        assert!(ensure_url_safe("https://8.8.8.8/").await.is_ok());
    }

    #[tokio::test]
    async fn ensure_url_safe_rejects_credentials() {
        assert!(
            ensure_url_safe("https://user:pass@example.com/")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ensure_url_safe_fail_closed_on_dns_error() {
        // Non-existent hostname must fail closed, not silently allow.
        // This proves we do not swallow DNS errors for browser tier.
        let res = ensure_url_safe("https://this-host-does-not-exist-12345.invalid/").await;
        assert!(res.is_err(), "non-resolvable host must fail closed, got Ok");
        let msg = res.unwrap_err().to_string().to_lowercase();
        assert!(
            msg.contains("dns") || msg.contains("fail-closed") || msg.contains("blocked"),
            "error must mention DNS/fail-closed, got: {msg}"
        );
    }

    // ---- fake-ip / RFC 2544 tiering (issue #83) ----

    #[test]
    fn rfc2544_resolved_tier_allows_fake_ip_block() {
        // Fake-ip TUN networks (mihomo/Clash/Surge) resolve every
        // hostname into 198.18.0.0/15. The resolved tier must pass
        // those; the literal tier must keep blocking them.
        for addr in [
            "198.18.0.0",
            "198.18.0.25",
            "198.18.255.255",
            "198.19.0.1",
            "198.19.255.254",
        ]
        .iter()
        .map(|a| a.parse::<IpAddr>().unwrap())
        {
            assert!(
                !is_ssrf_resolved_ip(&addr),
                "resolved tier must allow fake-ip fabric address {addr}"
            );
            assert!(is_ssrf_ip(&addr), "literal tier must still block {addr}");
        }
    }

    #[test]
    fn resolved_tier_still_blocks_real_private_ranges() {
        for addr in [
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "192.88.99.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "2001:db8::1",
        ]
        .iter()
        .map(|a| a.parse::<IpAddr>().unwrap())
        {
            assert!(
                is_ssrf_resolved_ip(&addr),
                "resolved tier must keep blocking {addr}"
            );
        }
    }

    #[test]
    fn private_egress_policy_controls_literal_guard() {
        for target in [
            "http://127.0.0.1/",
            "http://198.18.0.25/",
            "http://[::ffff:169.254.169.254]/",
        ] {
            assert!(validate_url_basic_with_policy(target, true).is_ok());
            assert!(validate_url_basic_with_policy(target, false).is_err());
        }
    }
}
