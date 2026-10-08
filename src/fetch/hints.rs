//! High-entropy client hints (WICG UA-CH).
//!
//! The low-entropy trio (sec-ch-ua / -mobile / -platform) rides every
//! request from the profile. This module carries the ACCEPTED hints: an
//! origin opts in with `Accept-CH` and subsequent requests to that origin
//! send the accepted hints, like Chrome. The cache is session-scoped
//! (in memory, bounded TTL and entry count): an origin-visit trail never
//! reaches disk, and no hint is ever sent preemptively.

use crate::profile::BrowserProfile;

/// High-entropy hints this client derives honestly from its own identity.
/// Anything else an origin accepts is ignored.
pub const SUPPORTED: [&str; 4] = [
    "sec-ch-ua-full-version-list",
    "sec-ch-ua-arch",
    "sec-ch-ua-bitness",
    "sec-ch-ua-model",
];

/// True for origins where Chrome sends hints: https, or a potentially
/// trustworthy loopback origin (localhost / 127.0.0.0/8 / ::1).
pub fn secure_context(is_https: bool, host: &str) -> bool {
    if is_https {
        return true;
    }
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

#[derive(Default)]
pub struct AcceptChCache {
    entries: Vec<Entry>,
}

struct Entry {
    origin: String,
    stored: u64,
    hints: Vec<&'static str>,
}

impl AcceptChCache {
    const TTL_SECS: u64 = 12 * 3600;
    const CAP: usize = 256;

    /// Record the response's Accept-CH for `origin`. The header names the
    /// full set the origin wants (replace, not union); a response without
    /// Accept-CH changes nothing, and a set with no supported hint clears
    /// the origin (all it wants we cannot honestly send).
    pub fn record(&mut self, origin: &str, headers: &[(String, String)], now: u64) {
        let mut saw = false;
        let mut hints: Vec<&'static str> = Vec::new();
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("accept-ch") {
                continue;
            }
            saw = true;
            for part in value.split(',') {
                let hint = part.trim().to_ascii_lowercase();
                if let Some(known) = SUPPORTED.iter().find(|h| **h == hint.as_str())
                    && !hints.contains(known)
                {
                    hints.push(known);
                }
            }
        }
        if !saw {
            return;
        }
        self.entries.retain(|e| e.origin != origin);
        if !hints.is_empty() {
            self.entries.push(Entry {
                origin: origin.to_owned(),
                stored: now,
                hints,
            });
            while self.entries.len() > Self::CAP {
                self.entries.remove(0);
            }
        }
    }

    /// The hints this origin may receive right now (unexpired).
    pub fn hints_for(&self, origin: &str, now: u64) -> Vec<&'static str> {
        self.entries
            .iter()
            .find(|e| e.origin == origin && now.saturating_sub(e.stored) < Self::TTL_SECS)
            .map(|e| e.hints.clone())
            .unwrap_or_default()
    }
}

/// The wire headers for `hints`, derived from the profile so every value
/// stays coherent with the low-entropy trio the same request carries.
pub fn hint_headers(profile: &BrowserProfile, hints: &[&'static str]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(hints.len());
    for hint in hints {
        match *hint {
            "sec-ch-ua-full-version-list" => {
                if let Some(list) = full_version_list(&profile.sec_ch_ua) {
                    out.push(((*hint).to_string(), list));
                }
            }
            "sec-ch-ua-arch" => {
                let _ = profile.platform;
                // The UA tokens present x86_64/Intel on every supported
                // platform, so x86 is the coherent arch; "arm" would
                // contradict the same request's User-Agent.
                out.push(((*hint).to_string(), "\"x86\"".to_string()));
            }
            "sec-ch-ua-bitness" => out.push(((*hint).to_string(), "\"64\"".to_string())),
            "sec-ch-ua-model" => out.push(((*hint).to_string(), "\"\"".to_string())),
            _ => {}
        }
    }
    out
}

/// `sec-ch-ua` brands with the full version: Chrome's full-version-list
/// carries the same brands in the same order, each `v="N"` padded to
/// `v="N.0.0.0"` (the identity everywhere else presents the same major).
fn full_version_list(sec_ch_ua: &str) -> Option<String> {
    let mut entries = Vec::new();
    for entry in sec_ch_ua.split(',') {
        let (brand, rest) = entry.trim().split_once(";v=")?;
        let version = rest.trim().trim_matches('"');
        entries.push(format!(
            "{brand};v=\"{version}{}\"",
            if version.contains('.') { "" } else { ".0.0.0" }
        ));
    }
    (!entries.is_empty()).then(|| entries.join(", "))
}

/// True when `Critical-CH` names a supported hint the request did not
/// already carry (the server says the representation is incomplete
/// without it).
pub fn critical_missing(critical: &str, sent: &[String]) -> bool {
    critical.split(',').any(|part| {
        let name = part.trim().to_ascii_lowercase();
        SUPPORTED.contains(&name.as_str()) && !sent.iter().any(|n| n.eq_ignore_ascii_case(&name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Platform;

    fn h(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn accept_ch_cache_replaces_and_expires_per_origin() {
        let mut cache = AcceptChCache::default();
        let origin = "https://example.com";
        cache.record(
            origin,
            &[h(
                "accept-ch",
                "Sec-CH-UA-Full-Version-List, sec-ch-ua-arch, x-unknown",
            )],
            100,
        );
        assert_eq!(
            cache.hints_for(origin, 100),
            ["sec-ch-ua-full-version-list", "sec-ch-ua-arch"]
        );
        // A later Accept-CH names the full set: replace, not union.
        cache.record(origin, &[h("accept-ch", "sec-ch-ua-bitness")], 200);
        assert_eq!(cache.hints_for(origin, 200), ["sec-ch-ua-bitness"]);
        // An Accept-CH with no supported hint clears the origin.
        cache.record(origin, &[h("accept-ch", "sec-ch-ua-platform-version")], 300);
        assert!(cache.hints_for(origin, 300).is_empty());
        // TTL.
        cache.record(origin, &[h("accept-ch", "sec-ch-ua-model")], 400);
        assert_eq!(cache.hints_for(origin, 400), ["sec-ch-ua-model"]);
        assert!(
            cache
                .hints_for(origin, 400 + AcceptChCache::TTL_SECS)
                .is_empty()
        );
        // Other origins never inherit.
        assert!(cache.hints_for("https://other.example", 400).is_empty());
    }

    #[test]
    fn accept_ch_cache_is_bounded() {
        let mut cache = AcceptChCache::default();
        for i in 0..(AcceptChCache::CAP + 44) {
            cache.record(
                &format!("https://o{i}.example"),
                &[h("accept-ch", "sec-ch-ua-arch")],
                i as u64,
            );
        }
        let live = (0..(AcceptChCache::CAP + 44))
            .filter(|i| {
                !cache
                    .hints_for(&format!("https://o{i}.example"), 0)
                    .is_empty()
            })
            .count();
        assert_eq!(live, AcceptChCache::CAP);
        // The OLDEST entries were the ones evicted.
        assert!(cache.hints_for("https://o0.example", 0).is_empty());
        assert!(
            !cache
                .hints_for(&format!("https://o{}.example", AcceptChCache::CAP + 43), 0)
                .is_empty()
        );
    }

    #[test]
    fn full_version_list_pads_the_same_brands() {
        let branded = BrowserProfile::chrome(150, Platform::Linux, true);
        assert_eq!(
            full_version_list(&branded.sec_ch_ua).unwrap(),
            "\"Chromium\";v=\"150.0.0.0\", \"Not=A?Brand\";v=\"99.0.0.0\", \"Google Chrome\";v=\"150.0.0.0\""
        );
        let unbranded = BrowserProfile::chrome(151, Platform::Linux, false);
        let list = full_version_list(&unbranded.sec_ch_ua).unwrap();
        assert!(!list.contains("Google Chrome"));
        assert!(list.contains("\"Chromium\";v=\"151.0.0.0\""));
    }

    #[test]
    fn hint_values_stay_coherent_with_the_identity() {
        for platform in [Platform::Linux, Platform::Windows, Platform::MacOs] {
            let profile = BrowserProfile::chrome(150, platform, true);
            let headers = hint_headers(
                &profile,
                &[
                    "sec-ch-ua-full-version-list",
                    "sec-ch-ua-arch",
                    "sec-ch-ua-bitness",
                    "sec-ch-ua-model",
                ],
            );
            let get = |name: &str| {
                headers
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.as_str())
            };
            let full = get("sec-ch-ua-full-version-list").unwrap();
            for brand in profile.sec_ch_ua.split(", ") {
                let (_, rest) = brand.split_once(";v=\"").unwrap();
                let major = rest.trim_end_matches('"');
                assert!(full.contains(&format!(";v=\"{major}.0.0.0\"")), "{full}");
            }
            assert_eq!(get("sec-ch-ua-arch"), Some("\"x86\""));
            assert!(
                profile.user_agent.contains("x86_64")
                    || profile.user_agent.contains("x64")
                    || profile.user_agent.contains("Intel")
            );
            assert_eq!(get("sec-ch-ua-bitness"), Some("\"64\""));
            assert_eq!(get("sec-ch-ua-model"), Some("\"\""));
        }
        assert!(hint_headers(&BrowserProfile::chrome_150(Platform::Linux), &[]).is_empty());
        assert!(
            hint_headers(&BrowserProfile::chrome_150(Platform::Linux), &["x-other"]).is_empty()
        );
    }

    #[test]
    fn secure_context_is_https_or_loopback() {
        assert!(secure_context(true, "example.com"));
        assert!(secure_context(false, "localhost"));
        assert!(secure_context(false, "app.localhost"));
        assert!(secure_context(false, "127.0.0.1"));
        assert!(secure_context(false, "127.9.9.9"));
        assert!(secure_context(false, "::1"));
        assert!(!secure_context(false, "example.com"));
        assert!(!secure_context(false, "192.168.1.10"));
    }

    #[test]
    fn critical_missing_names_supported_unsent_hints() {
        let sent = vec!["sec-ch-ua".to_string(), "sec-ch-ua-platform".to_string()];
        assert!(critical_missing("sec-ch-ua-arch", &sent));
        assert!(critical_missing(
            "sec-ch-ua-arch, sec-ch-ua-platform-version",
            &sent
        ));
        assert!(!critical_missing("sec-ch-ua-platform-version", &sent));
        assert!(!critical_missing("x-unknown", &sent));
        let sent = vec!["sec-ch-ua-arch".to_string()];
        assert!(!critical_missing("sec-ch-ua-arch", &sent));
    }
}
