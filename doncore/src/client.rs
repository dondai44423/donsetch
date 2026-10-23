//! The product-facing construction path — how a consumer opens the engine.
//!
//! One contract, stated once:
//! - **Construction**: `chrome(persona, resolve)` gives the identity-tuned wire
//!   and the identity record; `open(persona, resolve, features)` adds the state
//!   plane (cache / warm jar / learned hosts) and returns the `Engine`.
//!   Documents go through `Engine::fetch` — the cache, the JS runtime and the
//!   learned hosts live there — and API / subresource calls go through
//!   `Wire::fetch` with the per-request knobs. One wire, two entry points, no
//!   second engine.
//! - **State root**: `state::state_root()` resolves `DONCORE_STATE_DIR` →
//!   `DONCORE_CACHE_DIR` → `DONSETCH_CACHE_DIR` → the per-OS default, so the
//!   swap re-points the engine at the product's own cache dir with no code
//!   change.
//! - **Personas**: `"154"` is the Chrome this build was measured against (the
//!   ClientHello-parity class); anything else is the stock 149 preset.
//! - **Per-request knobs** live on `WireReq`; their defaults keep the runtime
//!   path byte-identical, so the JS bridge never has to know about them.

use std::net::SocketAddr;

use wreq::IntoEmulation;
use wreq_util::{Emulation as UtilEmulation, Platform, Profile};

use crate::engine::Engine;
use crate::identity::Identity;
use crate::state::{Cache, HostStore, JarStore};
use crate::wire::{tune_emulation, Wire};

/// Which tenants of the state plane are on. All three default on — the dev CLI
/// maps `--no-cache/--no-jar/--no-hosts` here; the product maps its config.
#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub cache: bool,
    pub jar: bool,
    pub hosts: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            cache: true,
            jar: true,
            hosts: true,
        }
    }
}

/// The platform the engine claims: the one it actually runs on (DECIDED
/// cross-platform, 2026-10-18). Per-OS paths compile on every target; mac/win
/// runtime qualification owed (no host). Never hardcode one box for others.
pub fn host_platform() -> Platform {
    #[cfg(target_os = "linux")]
    {
        Platform::Linux
    }
    #[cfg(target_os = "macos")]
    {
        Platform::MacOS
    }
    #[cfg(target_os = "windows")]
    {
        Platform::Windows
    }
}

/// Fallback UAs only — the emulation preset supplies the real strings; these
/// exist so a missing header never fabricates a wrong-platform UA.
pub fn default_ua() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36"
    }
    #[cfg(target_os = "macos")]
    {
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36"
    }
    #[cfg(target_os = "windows")]
    {
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36"
    }
}

/// `sec-ch-ua-platform-version` fallback per OS (kernel / product / NT
/// version); the identity layer replaces it with the machine's measured value.
pub fn default_platform_version() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "6.6.0"
    }
    #[cfg(target_os = "macos")]
    {
        "14.0.0"
    }
    #[cfg(target_os = "windows")]
    {
        "10.0.0"
    }
}

/// The identity-tuned wire. `persona`: `"154"` for the installed-Chromium
/// parity class, anything else for the stock preset. `resolve` pins
/// host → address (tests, `--resolve`).
pub fn chrome(persona: &str, resolve: Vec<(String, SocketAddr)>) -> (Wire, Identity) {
    let util_em = UtilEmulation::builder()
        .profile(Profile::Chrome149)
        .platform(host_platform())
        .build();
    let full = if persona == "154" {
        tune_emulation(util_em.into_emulation())
    } else {
        util_em.into_emulation()
    };
    let ua = full
        .headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(default_ua())
        .to_string();
    let platform_ver = full
        .headers
        .get("sec-ch-ua-platform-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(default_platform_version())
        .trim_matches('"')
        .to_string();
    let ident = Identity::detect(&ua, &platform_ver);
    let wire = Wire::new(full, resolve);
    (wire, ident)
}

/// Opens the engine the way a product needs it: identity + wire + the state
/// plane under `state_root()`, with the due prune armed.
pub fn open(persona: &str, resolve: Vec<(String, SocketAddr)>, features: Features) -> Engine {
    let (wire, ident) = chrome(persona, resolve);
    if features.jar {
        if let Some(st) = JarStore::open() {
            wire.jar
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .attach_store(st);
        }
    }
    let cache = if features.cache { Cache::open() } else { None };
    let hosts = if features.hosts {
        HostStore::open()
    } else {
        None
    };
    let engine = Engine::new(wire, ident, cache, hosts);
    if let Some(c) = &engine.cache {
        c.prune_if_due();
    }
    engine
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_persona_picks_the_identity_class() {
        let (_, ident_154) = chrome("154", Vec::new());
        assert!(ident_154.ua.contains("Chrome/154"), "{}", ident_154.ua);
        let (_, ident_149) = chrome("149", Vec::new());
        assert!(ident_149.ua.contains("Chrome/149"), "{}", ident_149.ua);
        assert_ne!(ident_154.ua, ident_149.ua);
    }

    #[test]
    fn opening_without_stores_still_builds_the_engine() {
        let engine = open(
            "154",
            Vec::new(),
            Features {
                cache: false,
                jar: false,
                hosts: false,
            },
        );
        assert!(engine.cache.is_none());
        assert!(engine.hosts.is_none());
        assert!(engine.identity_json.contains("Chrome/154"));
    }

    /// The swap's one state-location contract: the product's cache dir steers
    /// the engine when its own vars are absent. This is the only test in the
    /// crate that writes process env, and nothing else reads these two names.
    #[test]
    fn the_state_root_follows_the_products_cache_dir() {
        let dir = std::env::temp_dir().join("doncore-client-root-test");
        std::env::remove_var("DONCORE_STATE_DIR");
        std::env::remove_var("DONCORE_CACHE_DIR");
        std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        assert_eq!(crate::state::state_root(), Some(dir.clone()));
        // The engine's own names still win over the product's.
        std::env::set_var("DONCORE_STATE_DIR", dir.join("own"));
        assert_eq!(crate::state::state_root(), Some(dir.join("own")));
        std::env::remove_var("DONCORE_STATE_DIR");
        std::env::remove_var("DONSETCH_CACHE_DIR");
    }
}
