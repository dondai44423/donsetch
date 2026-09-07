//! Per-domain persona store (v4 phase 0.3).
//!
//! Every stealth tool in the market randomizes per request. Modern
//! bot scoring (JA4 + behavioral + consistency layers) scores
//! LONGITUDINAL coherence: real users are the same browser, from the
//! same machine, for weeks. A persona is DonSeTch's per-domain
//! stable identity: one Chrome build pin, one OS pin, one timezone/
//! locale, one viewport, one entropy seed, held for the domain's
//! lifetime, re-minted only on burn (quarantine) or drift (the wire
//! profile bumped underneath it).
//!
//! Phase 0 scope: the store, the coherence checker, the mint/
//! quarantine lifecycle, and binding of the active global profile
//! as "persona v1" per domain. Phase 1 diverges header sets per
//! persona; phase 3 binds ghost entropy (canvas/WebGL/audio) and
//! proxy exits to the same records.
//!
//! Personas are route memory: the DONSETCH_NO_ROUTE_MEMORY and
//! DONSETCH_ROUTE_MEMORY_READONLY switches cover them too.

use serde::{Deserialize, Serialize};

/// The active wire identity a persona must agree with. Sourced from
/// the profile the fetcher actually sends (which itself probes the
/// installed browser, so ghost and tier 1 claim the same build).
#[derive(Clone, Debug)]
pub struct PersonaCaps {
    pub chrome_major: u32,
    pub platform: crate::profile::Platform,
    pub branded: bool,
}

impl PersonaCaps {
    /// Caps from the profile a fetcher actually sends.
    pub fn from_profile(profile: &crate::profile::BrowserProfile) -> Self {
        let chrome_major = profile
            .user_agent
            .split("Chrome/")
            .nth(1)
            .and_then(|rest| rest.split('.').next())
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or(150);
        Self {
            chrome_major,
            platform: profile.platform,
            branded: profile.sec_ch_ua.contains("Google Chrome"),
        }
    }
}

/// One domain's stable identity.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Persona {
    /// Unix seconds of mint.
    pub created_at: u64,
    /// Monotonic mint generation: a quarantined persona's
    /// replacement always has generation+1, so a burned identity
    /// can never accidentally resurface.
    pub generation: u32,
    /// Pinned Chrome major (must equal the wire profile's).
    pub chrome_major: u32,
    /// OS pin (drives UA token + Sec-CH-UA-Platform in phase 1).
    pub platform: String,
    /// Brand set pin: true = Google Chrome brands, false = bare
    /// Chromium (mirrors the installed browser).
    pub branded: bool,
    /// IANA timezone ("Europe/Berlin").
    pub tz: String,
    /// BCP-47-ish locale ("en-US").
    pub locale: String,
    /// Viewport for ghost tabs (phase 3).
    pub viewport: (u32, u32),
    /// Entropy seed for canvas/WebGL/audio (phase 3). Never 0.
    pub entropy_seed: u64,
    /// Bound proxy exit id (phase 1+); a persona never crosses exits.
    pub proxy_id: Option<String>,
    /// Set when the persona was burned (detection event, drift);
    /// the next ensure re-mints.
    #[serde(default)]
    pub quarantined_at: Option<u64>,
    #[serde(default)]
    pub quarantine_reason: Option<String>,
}

/// Common real-user viewports; picked by entropy at mint.
const VIEWPORTS: [(u32, u32); 5] = [
    (1920, 1080),
    (1536, 864),
    (1366, 768),
    (2560, 1440),
    (1440, 900),
];

/// xorshift64*: persona entropy without a rand dep. Seed material
/// is host + mint time + generation; NOT a security primitive,
/// only a stable source of viewport/entropy divergence.
fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn seed_for(host: &str, created_at: u64, generation: u32) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in host.as_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= created_at.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h ^= u64::from(generation) << 32;
    let s = xorshift(h);
    // Never 0: 0 is the coherence checker's "never minted" marker.
    if s == 0 { 1 } else { s }
}

/// Persona timezone: the operator's own TZ when sane, else UTC.
/// (Phase 1 lets a declared proxy locale override per persona.)
fn env_tz() -> String {
    std::env::var("TZ")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| t.contains('/') && !t.contains(char::is_whitespace))
        .unwrap_or_else(|| "UTC".to_string())
}

/// Persona locale: LANG/LC_ALL ("en_US.UTF-8" -> "en-US"), else en-US.
fn env_locale() -> String {
    for var in ["LC_ALL", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            let base = v.split('.').next().unwrap_or("").replace('_', "-");
            let mut parts = base.split('-');
            let lang = parts.next().unwrap_or("");
            if lang.len() >= 2 && lang.len() <= 3 && lang.chars().all(|c| c.is_ascii_lowercase()) {
                return match parts.next() {
                    Some(region) if !region.is_empty() => format!("{lang}-{region}"),
                    _ => lang.to_string(),
                };
            }
        }
    }
    "en-US".to_string()
}

impl Persona {
    /// Mint a fresh persona for `host` under the current wire caps.
    pub fn mint(host: &str, caps: &PersonaCaps, generation: u32, now: u64) -> Self {
        let seed = seed_for(host, now, generation);
        Self {
            created_at: now,
            generation,
            chrome_major: caps.chrome_major,
            platform: format!("{:?}", caps.platform).to_lowercase(),
            branded: caps.branded,
            tz: env_tz(),
            locale: env_locale(),
            viewport: VIEWPORTS[(seed % VIEWPORTS.len() as u64) as usize],
            entropy_seed: seed,
            proxy_id: None,
            quarantined_at: None,
            quarantine_reason: None,
        }
    }

    /// Every layer must agree, or the persona is a tell. Returns
    /// the list of violations (empty = coherent).
    pub fn coherence_errors(&self, caps: &PersonaCaps) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some(reason) = &self.quarantine_reason {
            errors.push(format!("quarantined: {reason}"));
        }
        if self.entropy_seed == 0 {
            errors.push("entropy seed is 0 (never properly minted)".to_string());
        }
        // The headline check: the persona's claimed build must equal
        // the build the TLS/h2 layers actually emit. Drift here is
        // the classic JA4-vs-UA tell.
        if self.chrome_major != caps.chrome_major {
            errors.push(format!(
                "chrome drift: persona {} vs wire {}",
                self.chrome_major, caps.chrome_major
            ));
        }
        let caps_platform = format!("{:?}", caps.platform).to_lowercase();
        if self.platform != caps_platform {
            errors.push(format!(
                "platform mismatch: persona {} vs wire {}",
                self.platform, caps_platform
            ));
        }
        if self.branded != caps.branded {
            errors.push("brand set mismatch (Google Chrome vs bare Chromium)".to_string());
        }
        if self.tz.is_empty() || self.tz.contains(char::is_whitespace) {
            errors.push(format!("invalid timezone {:?}", self.tz));
        }
        if !valid_locale(&self.locale) {
            errors.push(format!("invalid locale {:?}", self.locale));
        }
        let (w, h) = self.viewport;
        if !(800..=4000).contains(&w) || !(600..=3000).contains(&h) {
            errors.push(format!("implausible viewport {w}x{h}"));
        }
        errors
    }

    pub fn coherent(&self, caps: &PersonaCaps) -> bool {
        self.coherence_errors(caps).is_empty()
    }
}

fn valid_locale(locale: &str) -> bool {
    let parts: Vec<&str> = locale.split('-').collect();
    if parts.len() > 2 {
        return false;
    }
    let lang = parts[0];
    if !(2..=3).contains(&lang.len()) || !lang.chars().all(|c| c.is_ascii_lowercase()) {
        return false;
    }
    match parts.get(1) {
        None => true,
        Some(region) => {
            (2..=4).contains(&region.len()) && region.chars().all(|c| c.is_ascii_alphabetic())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> PersonaCaps {
        PersonaCaps {
            chrome_major: 150,
            platform: crate::profile::Platform::Linux,
            branded: true,
        }
    }

    #[test]
    fn minted_persona_is_coherent() {
        let p = Persona::mint("example.com", &caps(), 1, 1_700_000_000);
        assert!(p.coherent(&caps()), "{:?}", p.coherence_errors(&caps()));
        assert_ne!(p.entropy_seed, 0);
        assert_eq!(p.generation, 1);
    }

    #[test]
    fn coherence_catches_each_layer() {
        let c = caps();
        let good = Persona::mint("example.com", &c, 1, 1_700_000_000);

        let mut drift = good.clone();
        drift.chrome_major = 149;
        assert!(
            drift
                .coherence_errors(&c)
                .iter()
                .any(|e| e.contains("chrome drift"))
        );

        let mut os = good.clone();
        os.platform = "windows".into();
        assert!(
            os.coherence_errors(&c)
                .iter()
                .any(|e| e.contains("platform mismatch"))
        );

        let mut brands = good.clone();
        brands.branded = false;
        assert!(
            brands
                .coherence_errors(&c)
                .iter()
                .any(|e| e.contains("brand set"))
        );

        let mut tz = good.clone();
        tz.tz = "not a zone".into();
        assert!(!tz.coherent(&c));

        let mut loc = good.clone();
        loc.locale = "EN_us_9".into();
        assert!(!loc.coherent(&c));

        let mut vp = good.clone();
        vp.viewport = (100, 100);
        assert!(!vp.coherent(&c));

        let mut seed = good.clone();
        seed.entropy_seed = 0;
        assert!(!seed.coherent(&c));

        let mut burned = good.clone();
        burned.quarantine_reason = Some("detected".into());
        assert!(
            burned
                .coherence_errors(&c)
                .iter()
                .any(|e| e.contains("quarantined"))
        );
    }

    #[test]
    fn different_hosts_get_divergent_entropy() {
        let a = Persona::mint("a.example", &caps(), 1, 1_700_000_000);
        let b = Persona::mint("b.example", &caps(), 1, 1_700_000_000);
        assert_ne!(a.entropy_seed, b.entropy_seed);
    }

    #[test]
    fn locale_parsing() {
        assert!(valid_locale("en-US"));
        assert!(valid_locale("de"));
        assert!(valid_locale("zh-Hant"));
        assert!(!valid_locale("EN-US"));
        assert!(!valid_locale("e"));
        assert!(!valid_locale("en-U"));
        assert!(!valid_locale("en-US-x"));
    }
}
