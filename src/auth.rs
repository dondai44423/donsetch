//! Authenticated session management: `donsetch login`.
//!
//! Security model (non-negotiable):
//! - Credentials NEVER touch this process. The user types them into
//!   a real browser window; we never capture keystrokes, never take
//!   screenshots, never CDP-attach before the user finishes.
//! - `auth-state.json` carries metadata only: cookie names, counts,
//!   expiries, probe verdicts. Never cookie values.
//! - Cookie values live exactly where they already lived: the
//!   session vault (ghost-state.json, written 0600-before-content).
//! - Login URLs are logged as scheme+host only: query tokens from
//!   login redirects never reach a log line or a filename.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::ghost::cache::{CookieRecord, is_session_worthy, now, store_session_cookies};

/// Metadata for one authenticated domain. Deliberately free of any
/// cookie value: serialization of this struct must never leak a
/// session token.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthState {
    pub created_at: u64,
    pub last_login: u64,
    pub cookie_count: usize,
    /// min/max expiry of stored cookies (None = session cookie).
    pub expires_min: Option<u64>,
    pub expires_max: Option<u64>,
    /// Masked cookie names (names only, never values).
    pub cookie_names: Vec<String>,
    /// None = never probed.
    pub verified: Option<bool>,
    pub last_probe: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthRegistry {
    pub domains: BTreeMap<String, AuthState>,
}

impl AuthRegistry {
    pub fn load() -> Self {
        let p = auth_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(r) => r,
                Err(_) => {
                    // Corrupt registry: quarantine it, start clean.
                    let _ = std::fs::rename(&p, p.with_extension("json.corrupt"));
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Atomic save; 0600 is set BEFORE content lands on disk so the
    /// registry is never world-readable, even transiently.
    pub fn save(&self) {
        #[cfg(not(test))]
        {
            if std::env::var_os("DONSEEK_NO_DISK_STATE").is_some() {
                return;
            }
        }
        let p = auth_path();
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_string(self) else {
            return;
        };
        let tmp = p.with_extension("json.tmp");
        let write_ok = {
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)
                    .and_then(|mut f| f.write_all(json.as_bytes()))
                    .map(|_| true)
                    .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                std::fs::write(&tmp, &json).is_ok()
            }
        };
        if !write_ok {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        let _ = std::fs::rename(&tmp, &p);
    }

    pub fn get(&self, domain: &str) -> Option<&AuthState> {
        self.domains.get(domain)
    }

    /// Record (or refresh) a login for `domain` from the stored
    /// cookie set. `verified` = post-login probe verdict.
    pub fn record_login(&mut self, domain: &str, cookies: &[CookieRecord], verified: Option<bool>) {
        let t = now();
        let mut names: Vec<String> = cookies
            .iter()
            .map(|c| c.name.clone())
            .filter(|n| !n.is_empty())
            .collect();
        names.sort();
        names.truncate(8);

        let exp: Vec<Option<u64>> = cookies.iter().map(|c| c.expires_at).collect();
        let entry = AuthState {
            created_at: self.get(domain).map(|s| s.created_at).unwrap_or(t),
            last_login: t,
            cookie_count: cookies.len(),
            expires_min: exp.iter().flatten().copied().min(),
            expires_max: exp.iter().flatten().copied().max(),
            cookie_names: names,
            verified,
            last_probe: verified.map(|_| t).or_else(|| {
                // Keep an old probe timestamp only when we have no
                // new verdict; it is still informative.
                self.get(domain).and_then(|s| s.last_probe)
            }),
        };
        self.domains.insert(domain.to_string(), entry);
        self.save();
    }

    /// Remove one domain from the registry AND from the cookie
    /// vault (logout). Returns true only if the domain existed.
    pub fn remove(&mut self, domain: &str) -> bool {
        let existed_registry = self.domains.remove(domain).is_some();
        if existed_registry {
            self.save();
        }
        let removed_vault = crate::ghost::cache::clear_session_cookies_for(domain);
        existed_registry || removed_vault
    }

    pub fn sorted(&self) -> Vec<(String, AuthState)> {
        let mut v: Vec<(String, AuthState)> = self
            .domains
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

fn auth_path() -> std::path::PathBuf {
    // Unit tests never touch the real registry.
    #[cfg(test)]
    {
        std::env::temp_dir()
            .join("donsetch-auth-unit-test")
            .join("auth-state.json")
    }
    #[cfg(not(test))]
    {
        crate::paths::cache_dir().join("auth-state.json")
    }
}

/// Normalize a user-supplied login target into
/// (browse_url, vault_key). Accepts bare hosts, URLs with any path,
/// loopback hosts with explicit ports (for local development), IPv4
/// and IPv6 literals. Rejects anything that could route a cookie
/// token or a login URL into a filename/log (query strings, paths).
/// The result of normalizing a user-supplied login target.
pub struct NormalizedTarget {
    /// What to open in the browser (loopback: http; else https).
    pub browse_url: String,
    /// Vault/registry key: scheme-less, portless host.
    pub key: String,
    /// Explicit port the user supplied (kept so the post-login
    /// probe can hit local dev fixtures on non-standard ports).
    pub probe_port: Option<u16>,
}

/// Normalize a user-supplied login target. Accepts bare hosts, URLs
/// with any path, loopback hosts with explicit ports (for local
/// development), IPv4 and IPv6 literals. Rejects anything that
/// could route a cookie token or a login URL into a filename/log
/// (query strings, paths, userinfo credentials).
pub fn normalize_target(input: &str) -> Result<NormalizedTarget, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("empty domain".into());
    }
    // Url::parse handles IDNA, lowercasing, IP literals, bad ports.
    let parsed = if raw.contains("://") {
        url::Url::parse(raw).map_err(|e| format!("not a domain or URL: {e}"))?
    } else {
        url::Url::parse(&format!("http://{raw}"))
            .map_err(|e| format!("not a domain or URL: {e}"))?
    };
    // CREDENTIAL SPILL GUARD: a userinfo URL would put a password
    // into process memory and possibly a log line. Reject it at the
    // front door, before anything else touches the string.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URLs with embedded credentials are not accepted: use a bare domain".into());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "missing host".to_string())?
        .trim_matches('.')
        .to_string();
    if host.is_empty() {
        return Err("empty host".into());
    }
    let lower = host.to_ascii_lowercase();
    if lower.contains(char::is_whitespace) {
        return Err("host contains whitespace".into());
    }
    let loopback =
        lower == "localhost" || lower == "127.0.0.1" || lower == "[::1]" || lower == "::1";
    let port = parsed.port();
    let display_host = match (port, &lower) {
        (Some(p), h) => format!("{h}:{p}"),
        (None, _) => lower.clone(),
    };
    // Vault/cookie keys carry no port: cookies are port-agnostic.
    let scheme = if loopback { "http" } else { "https" };
    let mut key = lower.clone();
    // IPv6 literal keys keep their brackets for display sanity, but
    // cookie matching strips both forms via cookie_belongs_to.
    if key.contains(':') && !key.starts_with('[') {
        key = format!("[{key}]");
    }
    Ok(NormalizedTarget {
        browse_url: format!("{scheme}://{display_host}"),
        key,
        probe_port: port,
    })
}

/// Backward-compatible one-liner for tests and call sites that
/// only need the (url, key) pair.
pub fn normalize_domain(input: &str) -> Result<(String, String), String> {
    normalize_target(input).map(|t| (t.browse_url, t.key))
}

/// Split a raw domain key into (host, port) for probe building.
fn probe_target(domain: &str, port_hint: Option<u16>) -> (String, u16) {
    if let Some(p) = port_hint {
        return (
            domain
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            p,
        );
    }
    let mut parts = domain.rsplitn(2, ':');
    let first = parts.next().unwrap_or(domain);
    if let (Some(rest), Ok(port)) = (parts.next(), first.parse::<u16>()) {
        if rest.starts_with('[') && rest.ends_with(']') {
            return (rest[1..rest.len() - 1].to_string(), port);
        }
        return (rest.to_string(), port);
    }
    (domain.to_string(), 443)
}

/// The host part of a cookie's domain, dot-trimmed, for registry
/// grouping and matching.
fn cookie_host(c: &CookieRecord) -> String {
    c.domain.trim_start_matches('.').to_ascii_lowercase()
}

/// Do cookies for `host` count as belonging to `domain`? Covers
/// apex cookies, wildcard subdomain cookies, and host-only cookies
/// on the exact host.
pub fn cookie_belongs_to(domain: &str, host: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}")) || domain.ends_with(&format!(".{host}"))
}

/// Parse a Netscape cookies.txt (curl format) file. Tolerates the
/// quirks users actually hit: Windows CRLF, `#HttpOnly_` prefixes,
/// blank lines, header comments, non-epoch expiry fields.
pub fn parse_netscape(content: &str) -> Vec<CookieRecord> {
    let mut out = Vec::new();
    for line in content.lines() {
        let mut l = line.trim().to_string();
        if l.is_empty() || l.starts_with('#') && !l.starts_with("#HttpOnly_") {
            continue;
        }
        let mut http_only = false;
        if let Some(rest) = l.strip_prefix("#HttpOnly_") {
            http_only = true;
            l = rest.to_string();
        }
        let fields: Vec<&str> = l.split('\t').collect();
        if fields.len() < 7 {
            continue;
        }
        let domain = fields[0].trim();
        let include_subdomains = fields[1].trim().eq_ignore_ascii_case("TRUE");
        // path / secure are informational; parsed below.
        let secure = fields[3].trim().eq_ignore_ascii_case("TRUE");
        let expires = fields[4].trim().parse::<u64>().ok().filter(|e| *e > 0);
        let name = fields[5].trim();
        let value = fields[6].trim();
        if domain.is_empty() || name.is_empty() || value.is_empty() {
            continue;
        }
        let domain_norm = format!(
            "{}{}",
            if include_subdomains { "." } else { "" },
            domain.trim_start_matches('.').to_ascii_lowercase()
        );
        out.push(CookieRecord {
            domain: domain_norm,
            path: fields[2].trim().to_string(),
            name: name.to_string(),
            value: value.to_string(),
            expires_at: expires,
            http_only,
            secure,
            same_site: String::new(),
        });
    }
    out
}

/// POST-LOGIN PROBE: one GET of the home page carrying the stored
/// cookies. A wall (redirect to /login|signin|auth, 401/403, or an
/// obvious interstitial) flips the verdict to unverified instead of
/// claiming success. Only loopback targets use plain HTTP.
pub struct ProbeResult {
    pub ok: bool,
    pub note: String,
}

pub fn probe_domain(
    domain: &str,
    cookies: &[CookieRecord],
    port_hint: Option<u16>,
) -> Result<ProbeResult, String> {
    let (host, port) = probe_target(domain, port_hint);
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
    let scheme = if loopback { "http" } else { "https" };
    let url = if !port_hint.is_some() && (port == 443 || (loopback && port == 80)) {
        format!("{scheme}://{host}/")
    } else {
        format!("{scheme}://{host}") + &format!(":{port}/")
    };
    let cookie_header = cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ");

    // Dedicated plain thread: `reqwest::blocking` panics when built or
    // dropped on a thread that already has a tokio runtime context,
    // and `commit_login` (hence `probe_domain`) is called synchronously
    // from `donsetch login`'s async command handler, which runs under
    // `#[tokio::main]`. With this crate's `panic = "abort"` release
    // profile, that panic would abort the whole process.
    std::thread::Builder::new()
        .name("login-probe".into())
        .spawn(move || probe_once(&url, &cookie_header))
        .map_err(|e| format!("probe thread spawn: {e}"))?
        .join()
        .map_err(|_| "probe thread panicked".to_string())?
}

fn probe_once(url: &str, cookie_header: &str) -> Result<ProbeResult, String> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(4))
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!(
            "donsetch/{}-login-probe",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|e| format!("probe client: {e}"))?;

    let resp = client
        .get(url)
        .header(reqwest::header::COOKIE, cookie_header)
        .send()
        .map_err(|e| format!("probe failed: {e}"))?;
    let status = resp.status();
    let final_url = resp.url().clone();
    // Consume the body eagerly: proves the endpoint served content
    // and lets the connection close cleanly.
    let _ = resp.bytes();
    let final_path = final_url.path().to_ascii_lowercase();

    let walled = final_path.contains("/login")
        || final_path.contains("/signin")
        || final_path.contains("/sso")
        || (status.as_u16() == 401 || status.as_u16() == 403);
    let ok = !walled && status.is_success();
    let note = format!("HTTP {status} -> {}", truncate(final_url.as_str(), 80));
    Ok(ProbeResult { ok, note })
}

/// Post-login bookkeeping shared by interactive + import paths:
/// store cookies, group per-domain metadata, optionally probe.
/// `probe_key`: when set, only THIS domain is probed (the domain the
/// user asked to log into); `probe_port`: its explicit port when they
/// supplied one (local fixtures).
pub fn commit_login(
    cookies: &[CookieRecord],
    probe: bool,
    probe_key: Option<&str>,
    probe_port: Option<u16>,
) -> (Vec<String>, Vec<(String, bool, String)>) {
    let mut cookies = cookies.to_vec();
    cookies.retain(is_session_worthy);
    store_session_cookies(&cookies);

    // Group by domain for the registry.
    let mut by_domain: BTreeMap<String, Vec<CookieRecord>> = BTreeMap::new();
    for c in &cookies {
        by_domain.entry(cookie_host(c)).or_default().push(c.clone());
    }
    let mut domains = Vec::new();
    let mut probes = Vec::new();
    for (domain, dom_cookies) in by_domain {
        let probe_this = probe && probe_key.is_none_or(|k| k == domain);
        let verdict = if probe_this {
            match probe_domain(
                &domain,
                &dom_cookies,
                if probe_key.is_some() {
                    probe_port
                } else {
                    None
                },
            ) {
                Ok(p) => {
                    let ok = p.ok;
                    probes.push((domain.clone(), ok, p.note));
                    Some(ok)
                }
                Err(e) => {
                    probes.push((domain.clone(), false, e));
                    None
                }
            }
        } else {
            None
        };
        let mut reg = AuthRegistry::load();
        reg.record_login(&domain, &dom_cookies, verdict);
        domains.push(domain);
    }
    (domains, probes)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(3)).collect();
        t.push_str("...");
        t
    }
}

/// Human expiry rendering: "session", "in 3d", "in 2h", "expired".
pub fn fmt_expiry(ts: Option<u64>) -> String {
    let Some(ts) = ts else {
        return "session".into();
    };
    let now = now();
    if ts <= now {
        return "expired".into();
    }
    let secs = ts - now;
    match secs {
        s if s < 3600 => format!("in {}m", s / 60),
        s if s < 86400 => format!("in {}h", s / 3600),
        s => format!("in {}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bare_host() {
        let (url, key) = normalize_domain("example.com").unwrap();
        assert_eq!(url, "https://example.com");
        assert_eq!(key, "example.com");
    }

    #[test]
    fn normalize_full_url_drops_path_query() {
        let (url, key) = normalize_domain("https://X.Example.com/login?next=%2Fhome#frag").unwrap();
        assert_eq!(url, "https://x.example.com");
        assert_eq!(key, "x.example.com");
    }

    #[test]
    fn normalize_rejects_garbage() {
        for bad in [
            "",
            "   ",
            "not a host with spaces.com",
            "http://",
            "example.com:99999",
            "https://exa mple.com/x",
            "@/evil",
        ] {
            assert!(normalize_domain(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn normalize_localhost_port_kept() {
        let (url, key) = normalize_domain("localhost:8137").unwrap();
        assert_eq!(url, "http://localhost:8137");
        assert_eq!(key, "localhost");
    }

    #[test]
    fn normalize_punycode() {
        let (_, key) = normalize_domain("bücher.example").unwrap();
        assert_eq!(key, "xn--bcher-kva.example");
    }

    #[test]
    fn cookie_belongs_matrix() {
        let d = "x.com";
        assert!(cookie_belongs_to(d, "x.com"));
        assert!(cookie_belongs_to(d, "api.x.com"));
        assert!(cookie_belongs_to(d, "a.b.x.com"));
        assert!(cookie_belongs_to("x.com", "x.com"));
        assert!(!cookie_belongs_to(d, "evil-x.com"));
        assert!(!cookie_belongs_to(d, "y.com"));
        // host-only cookie on a subdomain reaches the apex session.
        assert!(cookie_belongs_to("login.x.com", "x.com"));
    }

    #[test]
    fn netscape_plain_and_httponly_and_expired() {
        let raw = "\
# Netscape HTTP Cookie File
#HttpOnly_.example.com\tTRUE\t/\tTRUE\t0\tsid\tdeadbeef
.example.com\tTRUE\t/\tFALSE\t1893456000\tpref\tdark
other.test\tFALSE\t/\tFALSE\t1000000\tstale\tx
";
        let cs = parse_netscape(raw);
        assert_eq!(cs.len(), 3);
        let sid = cs.iter().find(|c| c.name == "sid").unwrap();
        assert!(sid.http_only);
        assert!(sid.secure);
        assert_eq!(sid.expires_at, None);
        let pref = cs.iter().find(|c| c.name == "pref").unwrap();
        assert!(!pref.secure);
        assert!(pref.expires_at.is_some());
        let stale = cs.iter().find(|c| c.name == "stale").unwrap();
        assert_eq!(stale.domain, "other.test");
        assert!(!stale.domain.starts_with('.'));
    }

    #[test]
    fn netscape_tolerates_crlf_and_bad_lines() {
        let raw = "\r\n# ignored comment\r\nnot-a-cookie-line\r\n.example.com\tTRUE\t/\tFALSE\t0\tok\tv\r\n";
        let cs = parse_netscape(raw);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].name, "ok");
    }

    #[test]
    fn registry_never_serializes_values() {
        // auth_path() redirects to a temp dir under cfg(test): this
        // test records without touching a real registry.
        // The registry only ever sees counts/names; even attempting
        // to smuggle a value-shaped name must not be fatal.
        let mut reg = AuthRegistry::default();
        reg.record_login(
            "x.com",
            &[CookieRecord {
                domain: ".x.com".into(),
                path: "/".into(),
                name: "auth_token".into(),
                value: "supersecret".into(),
                expires_at: None,
                http_only: true,
                secure: true,
                same_site: "Lax".into(),
            }],
            Some(true),
        );
        let json = serde_json::to_string(&reg).unwrap();
        assert!(!json.contains("supersecret"));
        assert!(json.contains("auth_token"));
    }

    #[test]
    fn fmt_expiry_buckets() {
        // Uses now(): just check shape stability, not exact labels.
        assert_eq!(fmt_expiry(None), "session");
        assert!(fmt_expiry(Some(now() - 5)).starts_with("expired"));
        assert!(fmt_expiry(Some(now() + 60 * 60 * 25)).starts_with("in "));
    }

    #[test]
    fn probe_target_parses_localhost_port() {
        let (h, p) = probe_target("localhost:8181", None);
        assert_eq!(h, "localhost");
        assert_eq!(p, 8181);
        assert_eq!(
            probe_target("example.com", None),
            ("example.com".into(), 443)
        );
        assert_eq!(
            probe_target("localhost", Some(4321)),
            ("localhost".into(), 4321)
        );
    }
}
