//! The identity portfolio.
//!
//! An identity is a *coherent* client: its TLS fingerprint, HTTP/2 settings,
//! header set and order, client hints and UA all come from one real profile, so
//! nothing presents a contradiction. We never patch a field to look like
//! something we are not; we choose among clients that already exist, and we
//! measure which one an origin accepts.
//!
//! This is the opposite of fingerprint parity work: there is no per-property
//! tuning here, only selection.

use std::sync::OnceLock;
use wreq::header::{HeaderName, HeaderValue};
use wreq::IntoEmulation;
use wreq_util::{Emulation, Platform, Profile};

/// Captured identity of the reference browser on this machine. It is *data*: the
/// user agent and client hints a real Chrome sends, so state acquired by that
/// browser (clearance, sessions) can be spent by the identity that acquired it.
pub struct Persona {
    pub user_agent: String,
    pub brands: String,
    pub platform: String,
    pub mobile: bool,
    pub accept_language: Option<String>,
    pub accept: Option<String>,
    /// The reference browser's WebGL strings and extensions, as its scripts read them.
    pub webgl: Option<serde_json::Value>,
    pub hardware_concurrency: Option<u64>,
    pub screen: Option<serde_json::Value>,
}

/// The chrome-local persona: the fields this crate reads, distilled from the
/// reference-browser capture (`~/Projects/donsetch-evidence/research/persona/chrome-local.json`) and built
/// into the binary, so it does not depend on the working directory.
const CHROME_LOCAL: &str = include_str!("../personas/chrome-local.json");

/// Google Chrome on Windows, captured the same way
/// (`~/Projects/donsetch-evidence/research/persona/chrome-win-156`).
const CHROME_WINDOWS: &str = include_str!("../personas/chrome-win.json");

/// `V5_PERSONA` names a full capture (or a distilled one) to use instead.
fn persona_source() -> Option<String> {
    match std::env::var("V5_PERSONA") {
        Ok(path) if !path.is_empty() => std::fs::read_to_string(path).ok(),
        _ => Some(CHROME_LOCAL.to_string()),
    }
}

/// The local reference browser's persona (`chrome-local`).
pub fn persona() -> Option<&'static Persona> {
    static PERSONA: OnceLock<Option<Persona>> = OnceLock::new();
    PERSONA
        .get_or_init(|| parse_persona(&persona_source()?))
        .as_ref()
}

/// The Windows reference browser's persona (`chrome-win`).
fn windows_persona() -> Option<&'static Persona> {
    static PERSONA: OnceLock<Option<Persona>> = OnceLock::new();
    PERSONA
        .get_or_init(|| parse_persona(CHROME_WINDOWS))
        .as_ref()
}

fn parse_persona(raw: &str) -> Option<Persona> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let navigator = &value["navigator"];
    let data = &navigator["userAgentData"];
    let brands = data["brands"]
        .as_array()
        .map(|brands| {
            brands
                .iter()
                .filter_map(|brand| {
                    Some(format!(
                        "\"{}\";v=\"{}\"",
                        brand["brand"].as_str()?,
                        brand["version"].as_str()?
                    ))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Some(Persona {
        user_agent: navigator["userAgent"].as_str()?.to_string(),
        brands,
        platform: data["platform"].as_str().unwrap_or("Linux").to_string(),
        mobile: data["mobile"].as_bool().unwrap_or(false),
        accept_language: value["requestHeaders"]["accept-language"]
            .as_str()
            .map(str::to_string),
        accept: value["requestHeaders"]["accept"]
            .as_str()
            .map(str::to_string),
        webgl: value.get("webgl").cloned(),
        hardware_concurrency: navigator["hardwareConcurrency"].as_u64(),
        screen: value.get("screen").cloned(),
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    /// A browser class, carried by a coherent emulation profile.
    Browser,
    /// A plain programmatic client: no browser pretense beyond its own headers.
    Plain,
}

pub struct Identity {
    /// Stable name used in state, scores and reports.
    pub name: &'static str,
    pub family: Family,
    /// Human-readable description of what this client claims to be.
    pub note: &'static str,
    profile: Option<(Profile, Platform)>,
    /// Extra request headers this class sends with a document request.
    extra: &'static [(&'static str, &'static str)],
}

impl Identity {
    /// The client stack this identity puts on the wire (TLS and HTTP/2
    /// shape): identities that share one differ only in headers, so a
    /// transport failure on one says nothing new about another of the class.
    pub fn wire_class(&self) -> &'static str {
        match self.profile.map(|(profile, _)| profile) {
            None => "native",
            Some(Profile::Firefox151) => "firefox",
            Some(Profile::Safari26_4) => "safari",
            Some(Profile::OkHttp5) => "okhttp",
            Some(_) => "chrome",
        }
    }

    pub fn emulation(&self) -> Option<wreq::Emulation> {
        let mut emulation = self.profile.map(|(profile, platform)| {
            Emulation::builder()
                .profile(profile)
                .platform(platform)
                .build()
                .into_emulation()
        })?;
        // The identities that carry a reference browser's own strings: a
        // clearance that browser earned is spendable by them.
        if let Some(persona) = self.reference() {
            let mut set = |name: &str, value: &str| {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(value),
                ) {
                    emulation.headers.insert(name, value);
                }
            };
            set("user-agent", &persona.user_agent);
            set("sec-ch-ua", &persona.brands);
            set("sec-ch-ua-mobile", if persona.mobile { "?1" } else { "?0" });
            set("sec-ch-ua-platform", &format!("\"{}\"", persona.platform));
            if let Some(language) = &persona.accept_language {
                set("accept-language", language);
            }
            if let Some(accept) = &persona.accept {
                set("accept", accept);
            }
        }
        Some(emulation)
    }

    /// The reference browser whose user agent and client hints this identity
    /// sends instead of its profile's, when it has one.
    ///
    /// Each one's TLS (JA4, peetprint), Akamai HTTP/2 fingerprint and header
    /// order were measured identical to its reference browser's first request,
    /// so the only thing the profile had older was the version it printed:
    /// `chrome-local` and the Linux `chrome` against Chromium 156 on Linux,
    /// `chrome-win` against Google Chrome 156 on Windows.
    fn reference(&self) -> Option<&'static Persona> {
        match self.name {
            "chrome-local" => persona(),
            "chrome" => persona().filter(|persona| {
                persona.platform == "Linux"
                    && !persona.mobile
                    && persona.user_agent.contains("Chrome/")
            }),
            "chrome-win" => windows_persona(),
            _ => None,
        }
    }

    pub fn headers(&self) -> Vec<(&'static str, &'static str)> {
        self.extra.to_vec()
    }

    /// What a page's script reads about this client: the strings its wire
    /// identity already sends (user agent, client hints, language), and the
    /// platform facts a script can query, chosen for the platform that user
    /// agent names. A challenge compares the two; they must never disagree.
    pub fn script_view(&self) -> serde_json::Value {
        let mut wire: Vec<(String, String)> = Vec::new();
        if let Some(emulation) = self.emulation() {
            for (name, value) in emulation.headers.iter() {
                if let Ok(value) = value.to_str() {
                    wire.push((name.as_str().to_string(), value.to_string()));
                }
            }
        }
        for (name, value) in self.extra {
            wire.retain(|(known, _)| known != name);
            wire.push((name.to_string(), value.to_string()));
        }
        let header = |name: &str| {
            wire.iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.as_str())
        };
        let user_agent = header("user-agent").unwrap_or("donsetch/5.0").to_string();
        let platform = Os::of(&user_agent);
        let local = if self.name == "chrome-local" {
            persona()
        } else {
            None
        };
        let brands = header("sec-ch-ua").map(parse_brands).unwrap_or_default();
        let languages = header("accept-language")
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|part| part.split(';').next())
                    .map(|tag| tag.trim().to_string())
                    .filter(|tag| !tag.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|tags| !tags.is_empty())
            .unwrap_or_else(|| vec!["en-US".into(), "en".into()]);
        let chromium = user_agent.contains("Chrome/");
        let (gpu_vendor, gpu_renderer) = platform.gpu();
        let mut webgl = serde_json::json!({
            "unmaskedVendor": gpu_vendor,
            "unmaskedRenderer": gpu_renderer,
        });
        if let Some(captured) = local.and_then(|persona| persona.webgl.as_ref()) {
            for key in ["unmaskedVendor", "unmaskedRenderer"] {
                if let Some(value) = captured.get(key).filter(|value| value.is_string()) {
                    webgl[key] = value.clone();
                }
            }
        }
        let (width, height) = platform.screen();
        let screen = local
            .and_then(|persona| persona.screen.clone())
            .unwrap_or_else(|| {
                serde_json::json!({
                    "width": width, "height": height, "availWidth": width,
                    "availHeight": height - platform.taskbar(), "colorDepth": 24, "pixelDepth": 24,
                    "availLeft": 0, "availTop": 0,
                })
            });
        serde_json::json!({
            "userAgent": user_agent,
            "appVersion": user_agent.strip_prefix("Mozilla/").unwrap_or(&user_agent),
            "platform": platform.navigator_platform(),
            "vendor": if chromium { "Google Inc." } else if user_agent.contains("Safari/") && !user_agent.contains("Firefox/") { "Apple Computer, Inc." } else { "" },
            "brands": brands,
            "uaPlatform": header("sec-ch-ua-platform").map(|value| value.trim_matches('"').to_string()).unwrap_or_else(|| platform.hint().to_string()),
            "mobile": header("sec-ch-ua-mobile") == Some("?1") || platform == Os::Android,
            "languages": languages,
            "hardwareConcurrency": local.and_then(|persona| persona.hardware_concurrency).unwrap_or(8),
            "maxTouchPoints": if platform == Os::Android { 5 } else { 0 },
            // Every desktop browser since 2023 lists the five PDF viewer
            // entries; mobile browsers list none.
            "pdfViewer": platform != Os::Android,
            "webgl": webgl,
            "screen": screen,
        })
    }
}

/// The operating system a user agent names, and the facts a script reads that
/// follow from it. These are each platform's common defaults, never the facts of
/// the machine running the fetcher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Os {
    Windows,
    Mac,
    Android,
    Linux,
}

impl Os {
    fn of(user_agent: &str) -> Os {
        if user_agent.contains("Windows") {
            Os::Windows
        } else if user_agent.contains("Android") {
            Os::Android
        } else if user_agent.contains("Macintosh") || user_agent.contains("Mac OS X") {
            Os::Mac
        } else {
            Os::Linux
        }
    }

    fn navigator_platform(self) -> &'static str {
        match self {
            Os::Windows => "Win32",
            Os::Mac => "MacIntel",
            Os::Android => "Linux armv81",
            Os::Linux => "Linux x86_64",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Os::Windows => "Windows",
            Os::Mac => "macOS",
            Os::Android => "Android",
            Os::Linux => "Linux",
        }
    }

    fn gpu(self) -> (&'static str, &'static str) {
        match self {
            Os::Windows => (
                "Google Inc. (Intel)",
                "ANGLE (Intel, Intel(R) UHD Graphics 620 (0x00005917) Direct3D11 vs_5_0 ps_5_0, D3D11)",
            ),
            Os::Mac => ("Google Inc. (Apple)", "ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)"),
            Os::Android => ("Google Inc. (Qualcomm)", "ANGLE (Qualcomm, Adreno (TM) 640, OpenGL ES 3.2)"),
            Os::Linux => ("Google Inc. (Intel)", "ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6)"),
        }
    }

    fn screen(self) -> (u32, u32) {
        match self {
            Os::Mac => (1512, 982),
            Os::Android => (412, 915),
            Os::Windows | Os::Linux => (1920, 1080),
        }
    }

    /// Height the system's own bars take from `availHeight`.
    fn taskbar(self) -> u32 {
        match self {
            Os::Windows => 48,
            Os::Mac => 25,
            Os::Android => 0,
            Os::Linux => 40,
        }
    }
}

/// `"Chromium";v="149", "Not:A-Brand";v="24"` as `[{brand, version}]`.
fn parse_brands(value: &str) -> Vec<serde_json::Value> {
    value
        .split(',')
        .filter_map(|part| {
            let (brand, version) = part.trim().split_once(";v=")?;
            Some(serde_json::json!({
                "brand": brand.trim_matches('"'),
                "version": version.trim_matches('"'),
            }))
        })
        .collect()
}

/// The portfolio. Order is the *prior*: cheapest and least presumptuous first,
/// then the largest browser class. Per-origin scores override this order after
/// the first encounter.
pub const PORTFOLIO: &[Identity] = &[
    Identity {
        name: "plain",
        family: Family::Plain,
        note: "plain HTTP client, transparent product UA",
        profile: None,
        extra: &[
            ("user-agent", "donsetch/5.0"),
            ("accept", "*/*"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
        ],
    },
    Identity {
        name: "chrome",
        family: Family::Browser,
        note: "Chrome 156 / Linux desktop, wire measured against the reference browser",
        profile: Some((Profile::Chrome149, Platform::Linux)),
        extra: &[
            // Measured against the reference browser's own document request:
            // these three are sent by Chrome and missing from the profile.
            ("upgrade-insecure-requests", "1"),
            ("sec-fetch-user", "?1"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
        ],
    },
    Identity {
        name: "chrome-local",
        family: Family::Browser,
        note: "Chrome 156 / Linux desktop, captured from the local reference browser",
        profile: Some((Profile::Chrome149, Platform::Linux)),
        extra: &[
            // Measured against the reference browser's own document request:
            // these three are sent by Chrome and missing from the profile.
            ("upgrade-insecure-requests", "1"),
            ("sec-fetch-user", "?1"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
        ],
    },
    Identity {
        name: "firefox",
        family: Family::Browser,
        note: "Firefox 151 / Linux desktop",
        profile: Some((Profile::Firefox151, Platform::Linux)),
        extra: &[],
    },
    Identity {
        name: "safari",
        family: Family::Browser,
        note: "Safari 26.4 / macOS desktop",
        profile: Some((Profile::Safari26_4, Platform::MacOS)),
        extra: &[],
    },
    Identity {
        name: "chrome-win",
        family: Family::Browser,
        note: "Chrome 156 / Windows desktop, wire measured against Chrome on Windows",
        profile: Some((Profile::Chrome149, Platform::Windows)),
        extra: &[
            // Measured against the reference browser's own document request:
            // these three are sent by Chrome and missing from the profile.
            ("upgrade-insecure-requests", "1"),
            ("sec-fetch-user", "?1"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
        ],
    },
    Identity {
        name: "chrome-android",
        family: Family::Browser,
        note: "Chrome 149 / Android phone",
        profile: Some((Profile::Chrome149, Platform::Android)),
        extra: &[],
    },
    Identity {
        name: "okhttp",
        family: Family::Browser,
        note: "OkHttp 5 / Android app client",
        profile: Some((Profile::OkHttp5, Platform::Android)),
        // OkHttp's own default agent. The emulation preset's agent names a real
        // third-party app; this client never speaks as someone else's product.
        extra: &[("user-agent", "okhttp/5.0.0")],
    },
];

pub fn lookup(name: &str) -> Option<&'static Identity> {
    PORTFOLIO.iter().find(|identity| identity.name == name)
}

/// Identity names in prior order.
pub fn names() -> Vec<&'static str> {
    PORTFOLIO.iter().map(|identity| identity.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chrome_local_persona_is_built_in() {
        // No working-directory dependence: the persona ships inside the binary.
        let persona = persona().expect("embedded persona");
        assert!(
            persona.user_agent.contains("Chrome/"),
            "{}",
            persona.user_agent
        );
        assert!(persona.brands.contains("Chromium"), "{}", persona.brands);
        assert!(persona.webgl.is_some() && persona.screen.is_some());
    }

    #[test]
    fn each_measured_chrome_identity_sends_its_reference_browsers_own_strings() {
        let persona = persona().expect("embedded persona");
        let header = |name: &str, header: &str| {
            let emulation = lookup(name).expect(name).emulation().expect("a browser");
            emulation
                .headers
                .get(header)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        for name in ["chrome", "chrome-local"] {
            assert_eq!(
                header(name, "user-agent").as_deref(),
                Some(persona.user_agent.as_str()),
                "{name}"
            );
            assert_eq!(
                header(name, "sec-ch-ua").as_deref(),
                Some(persona.brands.as_str()),
                "{name}"
            );
        }
        // Windows carries its own reference browser's strings, which name
        // its own platform, never the local one's.
        let windows = windows_persona().expect("embedded Windows persona");
        assert_eq!(
            header("chrome-win", "user-agent").as_deref(),
            Some(windows.user_agent.as_str())
        );
        assert_eq!(
            header("chrome-win", "sec-ch-ua").as_deref(),
            Some(windows.brands.as_str())
        );
        assert_eq!(
            header("chrome-win", "sec-ch-ua-platform").as_deref(),
            Some("\"Windows\"")
        );
        assert!(windows.user_agent.contains("Windows NT 10.0; Win64; x64"));
        assert!(windows.brands.contains("\"Google Chrome\""));
        assert_ne!(windows.user_agent, persona.user_agent);
        let android = header("chrome-android", "user-agent").unwrap_or_default();
        assert!(android.contains("Android"), "{android}");
    }

    #[test]
    fn each_identity_tells_scripts_what_its_headers_say() {
        for name in ["chrome", "chrome-win", "chrome-local", "firefox", "safari"] {
            let identity = lookup(name).expect(name);
            let view = identity.script_view();
            let ua = view["userAgent"].as_str().unwrap_or_default();
            let platform = view["platform"].as_str().unwrap_or_default();
            let expect = if ua.contains("Windows") {
                "Win32"
            } else if ua.contains("Mac OS X") {
                "MacIntel"
            } else {
                "Linux x86_64"
            };
            assert_eq!(platform, expect, "{name}: {ua}");
            if ua.contains("Chrome/") {
                assert!(
                    view["brands"]
                        .as_array()
                        .is_some_and(|brands| !brands.is_empty()),
                    "{name}: no brands"
                );
            }
        }
    }
}
