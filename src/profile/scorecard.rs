//! Stealth scorecard (v4 phase 0.4): the drift alarm.
//!
//! The market-wide stealth failure mode is DRIFT: Chrome bumps
//! every ~4 weeks, static mimicry rots, JA3/JA4 databases catch up,
//! and every tool that pinned a fingerprint last quarter quietly
//! becomes detectable. The scorecard makes drift impossible to
//! miss:
//!
//! - `donsetch doctor --stealth` captures the live fingerprint the
//!   fetcher emits right now (JA3, JA4, peetprint, Akamai h2,
//!   user agent) via a public echo endpoint and diffs every layer
//!   against the committed baseline fixture. Any layer that moved
//!   without a deliberate profile change = alarm.
//! - When a local Chrome is available, the same endpoint is ALSO
//!   captured through the ghost (a real browser): tier-1 vs real
//!   Chrome parity is the strongest stealth statement available,
//!   computed live with no fixture maintenance.
//! - CI runs the fixture diff weekly; a drift reds the pipeline
//!   before the ecosystem's JA4 databases even notice.
//!
//! The baseline fixture is recorded deliberately by the operator
//! (`--stealth-record`), never silently: a baseline refresh is a
//! reviewable event.

use serde::{Deserialize, Serialize};

use crate::fetch::client::Fetcher;

/// The public fingerprint echo endpoint (tls.peet.ws). Returns
/// JA3/JA4/peetprint/Akamai-h2 for the connecting client.
pub const ECHO_ENDPOINT: &str = "https://tls.peet.ws/api/all";

/// The fingerprint layers we track, one snapshot.
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FingerprintSnapshot {
    /// Free-text provenance: what produced this snapshot
    /// ("tier1 fetcher, profile chrome-151, linux", or the ghost
    /// chrome version). For human review of the fixture.
    /// Defaulted: raw echo endpoints do not carry it; capture()
    /// stamps it. Ghost-parsed snapshots carry it too.
    #[serde(default)]
    pub source: String,
    /// Defaulted like source
    #[serde(default)]
    pub captured_at: String,
    pub user_agent: String,
    pub ja3: String,
    pub ja3_hash: String,
    pub ja4: String,
    pub peetprint_hash: String,
    pub akamai_fingerprint: String,
    /// The profile name the fetcher claims to emit (drift between
    /// this and `source` is itself a finding).
    pub profile: String,
}

/// What one layer reported.
pub struct LayerVerdict {
    pub layer: &'static str,
    pub same: bool,
    pub baseline: String,
    pub live: String,
}

/// Capture the fingerprint the fetcher emits RIGHT NOW.
pub async fn capture(fetcher: &Fetcher) -> Result<FingerprintSnapshot, String> {
    let out = fetcher
        .fetch(ECHO_ENDPOINT)
        .await
        .map_err(|e| format!("echo endpoint fetch failed: {e}"))?;
    // Operator debug: when DONSETCH_DEBUG_ECHO=1 the RAW echo lands
    // in /tmp/ for offline diffing against the evergreen ghost
    // capture (/tmp/ghost-echo-blob.json). Off by default.
    if crate::config::env_flag("DONSETCH_DEBUG_ECHO") {
        let _ = std::fs::write("/tmp/tier1-echo-raw.json", &out.body);
    }
    parse_echo(&out.body, fetcher.profile().name)
}

/// Parse the echo endpoint's JSON into a snapshot.
pub fn parse_echo(body: &[u8], profile_name: &str) -> Result<FingerprintSnapshot, String> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| format!("echo endpoint returned non-JSON: {e}"))?;
    let tls = &v["tls"];
    let http2 = &v["http2"];
    let get = |path: &serde_json::Value, key: &str| -> Result<String, String> {
        path.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("echo payload missing {key}"))
    };
    Ok(FingerprintSnapshot {
        source: "tier1 fetcher".to_string(),
        captured_at: String::new(),
        user_agent: get(&v["http"], "user_agent")
            .or_else(|_| get(&v, "user_agent"))
            .unwrap_or_default(),
        ja3: get(tls, "ja3")?,
        ja3_hash: get(tls, "ja3_hash")?,
        ja4: get(tls, "ja4")?,
        peetprint_hash: get(tls, "peetprint_hash")?,
        akamai_fingerprint: get(http2, "akamai_fingerprint")?,
        profile: profile_name.to_string(),
    })
}

/// GREASE values (RFC 8701): every nibble pair 0x?A?A. Chrome
/// inserts random GREASE per connection, and permutes extension
/// order, so both must be normalized away before any comparison
/// (ja4's spec does the same internally, which is why ja4 is the
/// stable layer).
fn is_grease(v: u16) -> bool {
    // 0x0A0A, 0x1A1A, ..., 0xFAFA: both bytes identical, each
    // nibble 0xA.
    (v & 0x0f0f) == 0x0a0a && (v >> 8) == (v & 0xff)
}

/// Order- and GREASE-normalized ja3: the three list fields
/// (ciphers, extensions, groups) sorted numerically with GREASE
/// entries removed. Two captures of the same real configuration
/// compare equal; a configuration CHANGE (added/removed cipher or
/// extension) still diffs. This is the only honest way to compare
/// ja3 across connections.
fn normalize_ja3(ja3: &str) -> String {
    let fields: Vec<&str> = ja3.split(',').collect();
    if fields.len() != 5 {
        return ja3.to_string();
    }
    let norm_list = |list: &str| -> String {
        let mut items: Vec<u16> = list
            .split('-')
            .filter(|x| !x.is_empty())
            .filter_map(|x| x.parse::<u16>().ok())
            .filter(|v| !is_grease(*v))
            .collect();
        items.sort_unstable();
        items.dedup();
        items
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("-")
    };
    format!(
        "{},{},{},{},{}",
        fields[0],
        norm_list(fields[1]),
        norm_list(fields[2]),
        norm_list(fields[3]),
        fields[4]
    )
}

/// Per-layer diff of a live capture against a baseline. ja3_hash
/// is deliberately NOT a layer: it is order-sensitive and Chrome
/// permutes per connection, so the hash can never match twice.
pub fn diff(baseline: &FingerprintSnapshot, live: &FingerprintSnapshot) -> Vec<LayerVerdict> {
    let layer = |name: &'static str, base: &str, live_: &str| LayerVerdict {
        layer: name,
        same: base == live_,
        baseline: base.to_string(),
        live: live_.to_string(),
    };
    vec![
        layer("user agent", &baseline.user_agent, &live.user_agent),
        layer(
            "ja3 (normalized)",
            &normalize_ja3(&baseline.ja3),
            &normalize_ja3(&live.ja3),
        ),
        layer("ja4", &baseline.ja4, &live.ja4),
        layer("peetprint", &baseline.peetprint_hash, &live.peetprint_hash),
        layer(
            "akamai h2",
            &baseline.akamai_fingerprint,
            &live.akamai_fingerprint,
        ),
    ]
}

/// The committed baseline fixture, embedded at build time so the
/// installed binary carries it.
pub fn baseline_fixture() -> Result<FingerprintSnapshot, String> {
    parse_fixture_str(include_str!("../../tests/fixtures/stealth-baseline.json"))
}

pub fn parse_fixture_str(s: &str) -> Result<FingerprintSnapshot, String> {
    serde_json::from_str(s).map_err(|e| format!("stealth baseline fixture unreadable: {e}"))
}

/// Echo capture through the REAL local browser (ghost): the
/// always-current reference for `doctor --stealth --parity`. No
/// fixture involved: the comparison is tier-1 vs the browser on
/// the floor, and it cannot go stale. The echo JSON arrives
/// rendered in a <pre> block; carve the first JSON object out and
/// parse. `None` when no usable local Chrome exists (doctor says
/// so instead of pretending).
pub async fn capture_via_ghost(
    mgr: &std::sync::Arc<crate::ghost::manager::GhostManager>,
) -> Result<FingerprintSnapshot, String> {
    let profile = crate::profile::BrowserProfile::host_default();
    let mut g = mgr
        .acquire(&profile)
        .await
        .map_err(|e| format!("ghost acquire: {e}"))?;
    let page = crate::ghost::ops::ghost_fetch(&mut g, ECHO_ENDPOINT, Duration::from_secs(35))
        .await
        .map_err(|e| format!("ghost render of the echo: {e}"))?;
    let html = page.html;
    let start = html
        .find('{')
        .ok_or("no echo JSON found in the browser page")?;
    let end = html
        .rfind('}')
        .take_if(|e| *e > start)
        .ok_or("no echo JSON end in the browser page")?
        + 1;
    let blob = &html[start..end];
    let parse = parse_echo(blob.as_bytes(), "ghost real Chrome");
    if parse.is_err()
        && let Err(write_err) = std::fs::write("/tmp/ghost-echo-blob.json", blob.as_bytes())
    {
        eprintln!("ghost echo debug dump failed: {write_err}");
    }
    let mut snap = parse?;
    snap.source = "ghost (real local Chrome)".to_string();
    snap.captured_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ECHO_SAMPLE: &str = r#"{
        "ip": "203.0.113.9",
        "http_version": "h2",
        "method": "GET",
        "http": { "user_agent": "Mozilla/5.0 Chrome/151.0.0.0" },
        "tls": {
            "ja3": "771,4865-4866-4867,0-11-10,29-23-24,0",
            "ja3_hash": "b500e500c7416e5dd71b0d1b9384b91c",
            "ja4": "t13d1516h2_8daaf6152771_d8a2da3f94cd",
            "peetprint_hash": "2e0d4b0b5c0f9d963b8b4d68fc6840f3"
        },
        "http2": {
            "akamai_fingerprint": "1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p"
        }
    }"#;

    #[test]
    fn parses_echo_payload() {
        let snap = parse_echo(ECHO_SAMPLE.as_bytes(), "chrome-151").unwrap();
        assert_eq!(snap.ja4, "t13d1516h2_8daaf6152771_d8a2da3f94cd");
        assert_eq!(snap.user_agent, "Mozilla/5.0 Chrome/151.0.0.0");
        assert!(snap.akamai_fingerprint.starts_with("1:65536"));
    }

    #[test]
    fn missing_layer_is_an_error() {
        let broken = r#"{"tls": {"ja3": "x"}}"#;
        assert!(parse_echo(broken.as_bytes(), "chrome-151").is_err());
    }

    #[test]
    fn diff_flags_only_moved_layers() {
        let base = parse_echo(ECHO_SAMPLE.as_bytes(), "chrome-151").unwrap();
        let mut live = base.clone();
        live.ja4 = "t13d1516h2_deadbeef_deadbeef".to_string();
        let verdicts = diff(&base, &live);
        assert_eq!(verdicts.len(), 5);
        let moved: Vec<_> = verdicts.iter().filter(|v| !v.same).collect();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].layer, "ja4");
        assert_eq!(moved[0].baseline, "t13d1516h2_8daaf6152771_d8a2da3f94cd");
    }

    #[test]
    fn ja3_normalization_ignores_permutation_and_grease() {
        // Chrome permutes extension order per connection and
        // inserts random GREASE: two captures of the SAME config
        // must compare equal after normalization.
        let a = "771,4865-4866-4867,0-23-10-11-2570,29-23-24,0";
        let b = "771,4865-4866-4867,11-0-43690-23-10,29-23-24,0";
        // 2570 and 43690 are both GREASE (0x0A0A and 0xAAAA).
        assert_eq!(normalize_ja3(a), normalize_ja3(b));
        // A REAL change (extension 28 added) still diffs.
        let c = "771,4865-4866-4867,0-23-10-11-28,29-23-24,0";
        assert_ne!(normalize_ja3(a), normalize_ja3(c));
        // Malformed input passes through unchanged.
        assert_eq!(normalize_ja3("garbage"), "garbage");
    }

    #[test]
    fn committed_fixture_is_parseable() {
        // The fixture gates CI; an unreadable fixture must fail
        // loudly here, not silently in a weekly cron.
        baseline_fixture().unwrap();
    }
}
