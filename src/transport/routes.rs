//! Tier-1 transport route memory (v4 phase 5.1).
//!
//! Two persistent facts per origin in one JSON file:
//! 1. HTTP/3 capability, learned from the server's `alt-svc` response
//!    headers, with the `ma=` max-age lifetime. A failed direct h3
//!    attempt drops the entry, like Chrome's AltSvc cache.
//! 2. The serialized QUIC TLS 1.3 session (quiche's `Connection::
//!    session()` bytes), base64 in the same entry, for the next
//!    visit's resumption / 0-RTT.
//!
//! Local-only, same trust shape as the cookie vault: one file under
//! the cache dir, written only by the response of the same origin
//! (a never-writes-cross-origin rule); entries expire on `ma=`.
//! h3-29 and other draft identifiers never route: QUIC v1 only.
//! The egress fingerprint rides on every entry: a session learned
//! behind a proxy must never resume from a different egress.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
const MA_DEFAULT: u64 = 3600; // alt-svc without ma= gets Chrome's 1h default

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RouteState {
    use_h3: bool,
    h3_port: u16,
    /// Alt-svc ma= lifetime anchor (unix ms).
    valid_until_ms: u128,
    /// Egress fingerprint the route was measured on.
    egress: String,
    /// Serialized quiche SSL_SESSION, base64; empty when absent.
    session_b64: String,
}

#[derive(Default, Serialize, Deserialize)]
struct RoutesFile {
    version: u32,
    routes: HashMap<String, RouteState>,
}

struct RouteMemory {
    path: PathBuf,
    routes: HashMap<String, RouteState>,
}

impl RouteMemory {
    fn fresh(&mut self) {
        let f = load_file(&self.path);
        self.routes = f.routes;
    }
    fn persist(&self) {
        save_file(&self.path, &self.routes);
    }
}

static ROUTES: OnceLock<Arc<Mutex<RouteMemory>>> = OnceLock::new();

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn routes_path() -> PathBuf {
    let mut p = crate::paths::cache_dir();
    p.push("routes.json");
    p
}

fn load_file(path: &PathBuf) -> RoutesFile {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<RoutesFile>(&bytes) {
            Ok(f) if f.version == VERSION => f,
            _ => RoutesFile {
                version: VERSION,
                routes: HashMap::new(),
            },
        },
        Err(_) => RoutesFile {
            version: VERSION,
            routes: HashMap::new(),
        },
    }
}

fn save_file(path: &PathBuf, routes: &HashMap<String, RouteState>) {
    let bytes = match serde_json::to_vec(&RoutesFile {
        version: VERSION,
        routes: routes.clone(),
    }) {
        Ok(v) => v,
        Err(_) => return,
    };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn shared() -> Arc<Mutex<RouteMemory>> {
    ROUTES
        .get_or_init(|| {
            Arc::new(Mutex::new(RouteMemory {
                path: routes_path(),
                routes: HashMap::new(),
            }))
        })
        .clone()
}

/// Verify QUIC's + the QUIC's alt-svc value parse, e.g.
///   h3=":443"; ma=86400, h3-29=":443"; ma=86400
/// QUIC-v1's + a first-h3-candidate parse. Draft identifiers and
/// cross-host authorities never route in v1.
pub fn parse_h3_candidate(value: &str) -> Option<(u16, u64)> {
    for part in value.split(',') {
        let seg = part.trim();
        if !seg.to_ascii_lowercase().starts_with("h3") {
            continue;
        }
        let Some(after) = seg.get(2..) else { continue };
        if !after.starts_with('=') {
            continue; // h3-29 and quota-suffixed drafts never route
        }
        let (value_part, rest) = match after.find(';') {
            Some(p) => (after[..p].trim(), &after[p + 1..]),
            None => (after.trim(), ""),
        };
        let mut ma = MA_DEFAULT;
        for p in rest.split(';') {
            let pv = p.trim();
            if let Some(ma_str) = pv.strip_prefix("ma=")
                && let Ok(v) = ma_str.trim().parse::<u64>()
            {
                ma = v;
            }
        }
        let value_raw = value_part.trim().trim_start_matches('=');
        let inner = value_raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(value_raw);
        // v1: same-host only. ":443" == the default 443.
        let Some(port_part) = inner.strip_prefix(':') else {
            continue;
        };
        let Ok(port) = port_part.trim().parse::<u16>() else {
            continue;
        };
        return Some((port, ma));
    }
    None
}

pub fn h3_route(origin: &str, egress: &str) -> Option<u16> {
    let hit = with(|mem| {
        let entry = mem.routes.get(origin)?.clone();
        if !entry.use_h3 || entry.h3_port == 0 {
            return None;
        }
        if now_ms() >= entry.valid_until_ms {
            mem.routes.remove(origin);
            mem.persist();
            return None;
        }
        if entry.egress != egress {
            return None;
        }
        Some(entry.h3_port)
    });
    if std::env::var_os("DONGHOST_DEBUG").is_some() {
        eprintln!("[routes] h3_route({origin}) = {hit:?}");
    }
    hit
}

pub fn record_h3(origin: &str, port: u16, ma_secs: u64, egress: &str) {
    with(|mem| {
        let session = mem
            .routes
            .get(origin)
            .filter(|st| st.egress == egress)
            .map(|st| st.session_b64.clone());
        mem.routes.insert(
            origin.to_string(),
            RouteState {
                use_h3: true,
                h3_port: port,
                valid_until_ms: now_ms() + u128::from(ma_secs).saturating_mul(1000),
                egress: egress.to_string(),
                session_b64: session.unwrap_or_default(),
            },
        );
        mem.persist();
    });
}

pub fn drop_h3(origin: &str) {
    with(|mem| {
        if mem.routes.remove(origin).is_some() {
            mem.persist();
        }
    });
}

pub fn save_h3_session(origin: &str, egress: &str, session: &[u8]) {
    with(|mem| {
        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!(
                "[routes] save_h3_session key={origin} eg={egress} n={} match={}",
                mem.routes.len(),
                mem.routes
                    .get(origin)
                    .map(|s| s.egress.clone())
                    .unwrap_or_default()
            );
        }
        if let Some(st) = mem.routes.get_mut(origin)
            && st.egress == egress
        {
            st.session_b64 = B64.encode(session);
            mem.persist();
        }
    });
}

pub fn load_h3_session(origin: &str, egress: &str) -> Option<Vec<u8>> {
    with(|mem| {
        let st = mem.routes.get(origin)?;
        if st.egress != egress {
            return None;
        }
        if st.session_b64.is_empty() {
            return None;
        }
        B64.decode(&st.session_b64).ok()
    })
}

fn with<T>(f: impl FnOnce(&mut RouteMemory) -> T) -> T {
    let arc = shared();
    let mut mem = arc
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if mem.routes.is_empty() {
        mem.fresh();
    }
    f(&mut mem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_svc_h3_plain_port() {
        let (port, ma) = parse_h3_candidate("h3=\":443\"; ma=86400").expect("h3 parsed");
        assert_eq!(port, 443);
        assert_eq!(ma, 86400);
    }

    #[test]
    fn alt_svc_draft_and_crosshost_ignored() {
        assert!(parse_h3_candidate("h3-29=\":443\"; ma=86400").is_none());
        assert!(parse_h3_candidate("h3=\"example.com:443\"; ma=100").is_none());
        assert!(parse_h3_candidate("quic=\":443\"").is_none());
    }

    #[test]
    fn alt_svc_ma_default() {
        let (_, ma) = parse_h3_candidate("h3=\":443\"").unwrap();
        assert_eq!(ma, MA_DEFAULT);
    }

    #[test]
    fn alt_svc_no_quotes() {
        let (port, _) = parse_h3_candidate("h3=:443").unwrap();
        assert_eq!(port, 443);
    }

    #[test]
    fn multi_entries_pick_h3_v1() {
        let got = parse_h3_candidate(
            "h2.users, \"example.com:80\"; ma=1, h3=\":443\"; ma=600, h3-29=\":443\"",
        );
        assert_eq!(got, Some((443, 600)));
    }
}
