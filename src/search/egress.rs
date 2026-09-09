//! Egress pool + governor : the rate-limit solver.
//!
//! Rate limits are a BUDGET problem, not a rotation
//! problem. Rotation spreads the burn; this system never
//! exceeds what each lane can sustain:
//!
//! - LANE ROLES: proxies are workhorses; `direct` is the
//!   premium lane (the only egress that passes some
//!   engines, e.g. Brave). Direct serves at most ONE
//!   engine per query, reserved for engines whose proxy
//!   lanes are learned-burned.
//! - STRESS GAUGE: EWMA of recent outcomes. The caller
//!   reads it to shrink fan-out under pressure : you
//!   can't be rate-limited if you never exceed the rate.
//! - JITTERED PACING: a metronome is a bot signal.
//! - PREFLIGHT: dead/bad-auth proxies are probed at
//!   startup and never assigned mid-query.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
type PaceGate = Arc<tokio::sync::Mutex<Option<Instant>>>;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::transport::proxy::Proxy;

const BURN_COOLDOWN: Duration = Duration::from_secs(600);
const AUTH_BAN: Duration = Duration::from_secs(86_400); // 24h: wrong creds don't heal fast
const MIN_INTERVAL: Duration = Duration::from_millis(1200);
const JITTER_MS: u64 = 1300;
const DIRECT_MIN_INTERVAL: Duration = Duration::from_millis(2500);
const DIRECT_JITTER_MS: u64 = 2000;

/// Engines known to aggressively block proxy/datacenter IPs.
/// These prefer the direct lane even when proxies are
/// available : a 429/CAPTCHA from DDG or Brave on a proxy
/// is a wasted fan-out slot. Direct works for these engines
/// because our residential IP isn't on blocklists.
const PROXY_AVERSE: &[&str] = &["brave", "ddg"];

/// Health is shared by DDG's primary and alternate endpoints. This is not
/// ranking's index-family map: Bing and Yahoo keep their own egress health.
pub(super) fn health_key(engine: &str) -> &str {
    match engine {
        "ddg_lite" | "ddg_html" => "ddg",
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Suspect,
    Burned,
}

#[derive(Debug, Clone)]
pub struct Egress {
    /// "direct" or proxy id.
    pub id: String,
    pub proxy: Option<Proxy>,
}

struct PairState {
    health: Health,
    burned_until: Option<Instant>,
}

pub struct EgressPool {
    egresses: Vec<Egress>,
    pacing: Mutex<HashMap<(String, String), PaceGate>>,
    /// (engine, egress_id) -> state
    pairs: Mutex<HashMap<(String, String), PairState>>,
    /// Global proxy liveness (connect failures burn a proxy
    /// for ALL engines; a dead line is a dead line).
    dead: Mutex<HashMap<String, Instant>>,
    /// Stress gauge: consecutive-ish outcome EWMA
    /// (scaled x1000: 0 = all good, 1000 = everything fails).
    stress_ok: AtomicU32,
    stress_fail: AtomicU32,
}

/// Cheap non-crypto jitter from clock nanos (not security,
/// just cadence de-correlation).
fn jitter(max_ms: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % max_ms.max(1)
}

impl EgressPool {
    pub fn new(proxies: Vec<Proxy>) -> Self {
        let mut egresses = vec![Egress {
            id: "direct".into(),
            proxy: None,
        }];
        for p in proxies {
            egresses.push(Egress {
                id: p.id(),
                proxy: Some(p),
            });
        }
        Self {
            egresses,
            pacing: Mutex::new(HashMap::new()),
            pairs: Mutex::new(HashMap::new()),
            dead: Mutex::new(HashMap::new()),
            stress_ok: AtomicU32::new(2000), // seed optimistic
            stress_fail: AtomicU32::new(0),
        }
    }

    pub fn from_env() -> Self {
        Self::new(crate::transport::proxy::load_all())
    }

    /// All configured proxies (for preflight probing).
    pub fn proxies(&self) -> Vec<Proxy> {
        self.egresses
            .iter()
            .filter_map(|e| e.proxy.clone())
            .collect()
    }

    /// Stress gauge 0.0..1.0 (recent failure mass).
    pub fn stress(&self) -> f64 {
        let ok = self.stress_ok.load(Ordering::Relaxed) as f64;
        let fail = self.stress_fail.load(Ordering::Relaxed) as f64;
        fail / (ok + fail).max(1.0)
    }

    fn stress_record(&self, ok: bool) {
        // Decay then add: cheap EWMA over outcome counts.
        let decay = |v: u32| (v as f64 * 0.92) as u32;
        if ok {
            self.stress_ok.store(
                decay(self.stress_ok.load(Ordering::Relaxed)) + 1000,
                Ordering::Relaxed,
            );
            self.stress_fail.store(
                decay(self.stress_fail.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        } else {
            self.stress_fail.store(
                decay(self.stress_fail.load(Ordering::Relaxed)) + 1000,
                Ordering::Relaxed,
            );
            self.stress_ok.store(
                decay(self.stress_ok.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
    }

    /// Pick the healthiest egress for an engine.
    ///
    /// Lane policy:
    /// - engines whose proxy lanes are ALL burned get the
    ///   premium lane (direct) if `direct_available`
    /// - everyone else rides proxies first; direct only as
    ///   last resort (protect the home IP)
    pub fn pick(&self, engine: &str, exclude: &[String], direct_available: bool) -> Option<Egress> {
        let engine = health_key(engine);
        let pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dead = self
            .dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();

        let state_of = |id: &str| -> u8 {
            match pairs.get(&(engine.to_string(), id.to_string())) {
                None => 2, // unknown = optimistic
                Some(s) => match s.health {
                    Health::Healthy => 2,
                    Health::Suspect => 1,
                    Health::Burned => match s.burned_until {
                        Some(t) if t > now => 0,
                        _ => 1, // cooldown over: probation
                    },
                },
            }
        };
        let dead_globally = |id: &str| -> bool { dead.get(id).is_some_and(|&t| t > now) };

        // Are ALL proxy lanes burned for this engine?
        // For proxy-averse engines (Brave, etc.), pretend no
        // proxy is viable so the direct lane is preferred.
        let proxy_averse = PROXY_AVERSE.contains(&engine);
        let any_proxy_viable = if proxy_averse {
            false
        } else {
            self.egresses
                .iter()
                .filter(|e| e.proxy.is_some() && !exclude.contains(&e.id))
                .any(|e| !dead_globally(&e.id) && state_of(&e.id) > 0)
        };

        let mut best: Option<(&Egress, u8)> = None;
        for e in &self.egresses {
            if exclude.contains(&e.id) || dead_globally(&e.id) {
                continue;
            }
            let s = state_of(&e.id);
            if s == 0 {
                continue;
            }
            if e.proxy.is_none() {
                // The premium lane: only when offered AND
                // (proxies can't serve this engine) OR (no
                // proxy scored better).
                if !direct_available {
                    continue;
                }
                if any_proxy_viable {
                    continue;
                }
            }
            let score = s;
            if best.is_none_or(|(_, bs)| score > bs) {
                best = Some((e, score));
            }
        }
        // Direct is the fallback of last resort (e.g. all
        // proxies dead globally) : better a rested home IP
        // than a failed query.
        if best.is_none() && direct_available && !exclude.contains(&"direct".to_string()) {
            return self.egresses.first().cloned();
        }
        best.map(|(e, _)| e.clone())
    }

    /// Record a successful engine call through this egress.
    pub fn report_ok(&self, engine: &str, egress_id: &str) {
        let engine = health_key(engine);
        self.stress_record(true);
        let mut pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let s = pairs
            .entry((engine.to_string(), egress_id.to_string()))
            .or_insert(PairState {
                health: Health::Suspect,
                burned_until: None,
            });
        s.health = Health::Healthy;
        s.burned_until = None;
    }

    /// Engine rejected us (429 / challenge / empty parse):
    /// burn the pair, not the engine.
    pub fn report_blocked(&self, engine: &str, egress_id: &str) {
        let engine = health_key(engine);
        self.stress_record(false);
        let mut pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let s = pairs
            .entry((engine.to_string(), egress_id.to_string()))
            .or_insert(PairState {
                health: Health::Suspect,
                burned_until: None,
            });
        s.health = match s.health {
            Health::Healthy => Health::Suspect,
            _ => {
                s.burned_until = Some(Instant::now() + BURN_COOLDOWN);
                Health::Burned
            }
        };
    }

    /// The egress line itself is dead (connect failure).
    pub fn report_dead(&self, egress_id: &str) {
        self.stress_record(false);
        if egress_id == "direct" {
            return; // direct failure = network down; don't mark
        }
        self.dead
            .lock()
            .unwrap()
            .insert(egress_id.to_string(), Instant::now() + BURN_COOLDOWN);
    }

    /// Preflight guard: un-bench every lane (used when the
    /// probe endpoint itself died and burned all proxies).
    pub fn revive_all(&self) {
        self.dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// True when the pool has any proxy lanes at all : the
    /// no-proxy default changes lane policy (direct serves
    /// all engines, with strict pacing).
    pub fn has_proxies(&self) -> bool {
        self.egresses.len() > 1
    }

    /// Proxy auth failed (CONNECT 407): credentials wrong.
    /// Bench long : wrong creds don't heal by waiting.
    pub fn report_auth_fail(&self, egress_id: &str) {
        self.stress_record(false);
        self.dead
            .lock()
            .unwrap()
            .insert(egress_id.to_string(), Instant::now() + AUTH_BAN);
    }

    /// Pacing with jitter: this (engine, egress) pair is
    /// not hit more than once per randomized interval.
    /// The premium lane paces slower : protect the home IP.
    pub async fn pace(&self, engine: &str, egress_id: &str) {
        let (base, jit) = if egress_id == "direct" {
            (DIRECT_MIN_INTERVAL, DIRECT_JITTER_MS)
        } else {
            (MIN_INTERVAL, JITTER_MS)
        };
        let interval = base + Duration::from_millis(jitter(jit));
        let gate = {
            let mut gates = self
                .pacing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            gates
                .entry((engine.into(), egress_id.into()))
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
                .clone()
        };
        // FIFO, cancellation-safe admission. Waiters reserve no future slots.
        // Cancellation drops the guard without changing the last admission.
        let mut last = gate.lock().await;
        if let Some(at) = *last {
            tokio::time::sleep(interval.saturating_sub(at.elapsed())).await;
        }
        *last = Some(Instant::now());
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    #[test]
    fn regression_ddg_fallback_updates_the_selected_egress_health() {
        let proxy = Proxy::parse("http://127.0.0.1:12345").unwrap();
        let id = proxy.id();
        let pool = EgressPool::new(vec![proxy]);
        pool.report_blocked("ddg", &id);
        pool.report_ok("ddg_html", &id);
        assert_eq!(pool.pick("ddg", &[], false).map(|e| e.id), Some(id.clone()));
        pool.report_blocked("ddg_html", &id);
        pool.report_blocked("ddg_html", &id);
        pool.report_ok("yahoo", &id);
        assert!(pool.pick("ddg", &[], false).is_none());
        assert!(pool.pick("ddg_html", &[], false).is_none());
        assert!(pool.pick("yahoo", &[], false).is_some());
        pool.report_ok("ddg_lite", &id);
        assert!(pool.pick("ddg_html", &[], false).is_some());
    }

    #[tokio::test]
    async fn cancelled_waiters_do_not_reserve_future_slots() {
        let pool = EgressPool::new(Vec::new());
        pool.pace("google", "direct").await;
        let key = ("google".to_string(), "direct".to_string());
        let gate = pool.pacing.lock().unwrap()[&key].clone();
        let first = *gate.lock().await;
        for _ in 0..20 {
            assert!(
                tokio::time::timeout(Duration::from_millis(1), pool.pace("google", "direct"))
                    .await
                    .is_err()
            );
        }
        assert_eq!(*gate.lock().await, first);
        pool.report_ok("google", "direct");
        pool.report_blocked("google", "direct");
        assert_eq!(*gate.lock().await, first);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), pool.pace("bing", "direct"))
                .await
                .is_ok()
        );
    }

    #[test]
    fn direct_last_resort_is_the_same_for_all_engines() {
        let pool = EgressPool::new(Vec::new());
        for engine in ["google", "bing", "ddg", "brave"] {
            pool.report_blocked(engine, "direct");
            assert_eq!(pool.pick(engine, &[], true).unwrap().id, "direct");
        }
    }
}
