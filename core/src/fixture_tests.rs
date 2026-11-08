//! The three AWS WAF shapes, offline: the vendor's own captured bundle runs in the
//! engine against a local verifier that checks what Chrome's submission is
//! measured to carry.
//!
//! Captured from live pages (2026-11-06): goku (IMDb), legacy inline (Booking),
//! interstitial (Trustpilot). Each page's vendor URLs are pointed at the local
//! server, so nothing leaves the machine. The verifier cannot check the vendor's
//! server-side verdict; it checks the client contract: the signal decrypts under
//! the bundle's key, its CRC prefix equals the declared checksum, `domain` is the
//! page host (measured: a wrong `domain` is what AWS WAF refused), and the region
//! the challenge names is on the URL.
//!
//! The bundles are AWS's code, not this crate's license, so they are not in the
//! repository: the tests read them from `V5_FIXTURES` (default
//! `../core/fixtures`, the private tree) and are `#[ignore]`d so a checkout
//! without them reports them as not run, never as passed. Run them with
//! `cargo test --release -- --include-ignored`.

use crate::engine::{mint_within, Mint};
use crate::envelope::Limits;
use crate::identity;
use crate::testkit::{Reply, Request, Server};
use crate::webcrypto;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The token the fake verifier issues, shaped like a real one (uuid:mid:payload).
const TOKEN: &str = "00000000-0000-4000-8000-000000000000:HgoAAAAAAAAHAAAA:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
/// The bundle's signal key: a constant of the captured bundles, measured equal
/// in Chrome and in the engine (SubtleCrypto trace, 2026-11-06).
const SIGNAL_KEY: &str = "6f71a512b1e035eaab53d8be73120d3fb68a0ca346b9560aab3e5cdf753d5e98";

fn fixture(shape: &str, file: &str) -> Vec<u8> {
    let root = std::env::var_os("V5_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            [env!("CARGO_MANIFEST_DIR"), "..", "core", "fixtures"]
                .iter()
                .collect()
        });
    let path = root.join("aws-waf").join(shape).join(file);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unbase64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let Some(value) = ALPHABET.iter().position(|c| *c == byte) else {
            continue;
        };
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The submission's metadata: a multipart `solution_metadata` part (mp_verify)
/// or the whole JSON body (goku's verify).
fn metadata(request: &Request) -> Option<serde_json::Value> {
    let body = String::from_utf8_lossy(&request.body);
    if let Some(start) = body.find("name=\"solution_metadata\"\r\n\r\n") {
        let rest = &body[start + "name=\"solution_metadata\"\r\n\r\n".len()..];
        let end = rest.find("\r\n--")?;
        return serde_json::from_str(&rest[..end]).ok();
    }
    serde_json::from_str(&body).ok()
}

/// Everything the verifier found wrong with one submission; empty means it holds.
fn check(request: &Request, host: &str) -> Vec<String> {
    let mut wrong = Vec::new();
    let Some(meta) = metadata(request) else {
        return vec![format!("no metadata in {} bytes", request.body.len())];
    };
    if meta["domain"] != host {
        wrong.push(format!("domain {} != {host}", meta["domain"]));
    }
    if let Some(region) = meta["challenge"]["region"].as_str() {
        if request.path.contains("mp_verify") && !request.path.contains(&format!("region={region}"))
        {
            wrong.push(format!("region {region} missing from {}", request.path));
        }
    }
    let signal = meta["signals"][0]["value"]["Present"]
        .as_str()
        .unwrap_or_default();
    let Some((iv, sealed)) = signal.split_once("::") else {
        wrong.push(format!("signal shape {:.40}", signal));
        return wrong;
    };
    let params = format!(
        r#"{{"name":"AES-GCM","iv":"{}","tagLength":128}}"#,
        hex(&unbase64(iv))
    );
    let opened = webcrypto::framed("decrypt", &params, &unhex(SIGNAL_KEY), &unhex(sealed));
    if opened.first() != Some(&0) {
        wrong.push(format!(
            "signal does not decrypt: {}",
            String::from_utf8_lossy(&opened[1..])
        ));
        return wrong;
    }
    let plain = String::from_utf8_lossy(&opened[1..]).into_owned();
    let Some((declared, json)) = plain.split_once('#') else {
        wrong.push("signal has no checksum prefix".into());
        return wrong;
    };
    let computed = format!("{:08X}", crc32(json.as_bytes()));
    if declared != computed || meta["checksum"] != computed {
        wrong.push(format!(
            "checksum prefix {declared}, metadata {}, computed {computed}",
            meta["checksum"]
        ));
    }
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(fingerprint) if fingerprint.get("metrics").is_some() => {}
        _ => wrong.push("fingerprint is not the collector JSON".into()),
    }
    wrong
}

/// A named page host: the SDK derives `domain` from it through a public-suffix
/// walk, which an IP literal would short-circuit.
const HOST: &str = "www.fixture.test";

struct Run {
    here: String,
    mint: Mint,
    server: Server,
    verified: usize,
    wrong: Vec<String>,
}

/// Serve `shape` with its vendor origins pointed here, run the engine on it.
fn run(shape: &'static str, vendor_origins: &[&str]) -> Run {
    let verified = Arc::new(Mutex::new(0usize));
    let wrong = Arc::new(Mutex::new(Vec::<String>::new()));
    let origin = Arc::new(Mutex::new(String::new()));
    let (seen_ok, seen_wrong, seen_origin) = (
        Arc::clone(&verified),
        Arc::clone(&wrong),
        Arc::clone(&origin),
    );
    let server = Server::start(move |request| {
        let path = request
            .path
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();
        let host = seen_origin
            .lock()
            .unwrap()
            .trim_start_matches("http://")
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string();
        let has_token = request
            .header("cookie")
            .map(|cookie| cookie.contains("aws-waf-token="))
            .unwrap_or(false);
        if path.ends_with("/challenge.js") {
            Reply::ok(fixture(shape, "challenge.js"))
        } else if path.ends_with("/inputs") {
            Reply::ok(fixture(shape, "inputs.json"))
        } else if path.ends_with("/verify") || path.ends_with("/mp_verify") {
            let problems = check(request, &host);
            if problems.is_empty() {
                *seen_ok.lock().unwrap() += 1;
            }
            seen_wrong.lock().unwrap().extend(problems);
            Reply::ok(format!(r#"{{"token":"{TOKEN}","inputs":null}}"#))
        } else if path.ends_with("/interstitial/interstitial.js") {
            Reply::ok(fixture(shape, "interstitial.js"))
        } else if path == "/" && has_token {
            Reply::ok("<html><head><title>content</title></head><body>content</body></html>")
        } else if path == "/" {
            Reply::Full {
                status: 202,
                headers: Vec::new(),
                body: Vec::new(),
            }
        } else {
            Reply::ok("")
        }
    });
    crate::envelope::test_hosts::pin(HOST, server.addr.ip());
    let here = format!("http://{HOST}:{}", server.addr.port());
    *origin.lock().unwrap() = here.clone();
    let mut html = String::from_utf8(fixture(shape, "page.html")).expect("utf-8 page");
    for vendor in vendor_origins {
        html = html.replace(vendor, &here);
    }
    let limits = Limits {
        wall: Duration::from_millis(20_000),
        ..Limits::default()
    };
    let chrome = identity::lookup("chrome").expect("chrome identity");
    let mint = mint_within(&limits, &format!("{here}/"), &html, chrome, None).expect("mint");
    let verified = *verified.lock().unwrap();
    let wrong = wrong.lock().unwrap().clone();
    Run {
        here,
        mint,
        server,
        verified,
        wrong,
    }
}

fn assert_minted(run: &Run) {
    let here = &run.here;
    assert!(run.wrong.is_empty(), "verifier: {:?}", run.wrong);
    assert!(
        run.verified >= 1,
        "no submission reached the verifier: {:?}",
        run.server.hits()
    );
    let token = run
        .mint
        .cookies
        .iter()
        .find(|cookie| cookie.name == "aws-waf-token")
        .map(|cookie| cookie.value.as_str());
    assert_eq!(
        token,
        Some(TOKEN),
        "cookies: {:?}",
        run.mint.cookies.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    // Offline means offline: every request the page made came here.
    let away: Vec<&String> = run
        .mint
        .logs
        .iter()
        .filter(|line| line.starts_with("net ") && !line.contains(here.as_str()))
        .collect();
    assert!(away.is_empty(), "requests left the fixture: {away:?}");
}

#[test]
#[ignore = "needs the private AWS WAF captures (V5_FIXTURES)"]
fn goku_imdb_shape_mints_offline() {
    let _turn = crate::engine_tests::serial();
    let run = run(
        "goku",
        &["https://fb423e1ef94f.6277d64d.us-east-1.token.awswaf.com"],
    );
    assert_minted(&run);
}

#[test]
#[ignore = "needs the private AWS WAF captures (V5_FIXTURES)"]
fn legacy_inline_booking_shape_mints_offline() {
    let _turn = crate::engine_tests::serial();
    let run = run(
        "legacy",
        &["https://www.booking.com", "https://reports.booking.com"],
    );
    assert_minted(&run);
}

#[test]
#[ignore = "needs the private AWS WAF captures (V5_FIXTURES)"]
fn interstitial_trustpilot_shape_mints_and_probes_offline() {
    let _turn = crate::engine_tests::serial();
    let run = run(
        "interstitial",
        &["https://a7d575be72e8.edge.sdk.awswaf.com"],
    );
    assert_minted(&run);
    // The interstitial spends the token on the page itself before it reloads.
    let probes = run
        .server
        .hits()
        .iter()
        .filter(|path| path.as_str() == "/")
        .count();
    assert!(probes >= 1, "{:?}", run.server.hits());
}
