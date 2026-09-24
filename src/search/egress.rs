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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::transport::proxy::Proxy;

type PaceGate = Arc<tokio::sync::Mutex<Option<Instant>>>;

/// Process-wide pool (v4 A2). Installed once by the daemon so
/// ghost launch and doctor can share the same health world as
/// search/crawl/fetch without threading Arc through every path.
static GLOBAL: OnceLock<Arc<EgressPool>> = OnceLock::new();

/// Install the daemon's shared pool. First call wins; later calls
/// are ignored (tests construct private pools).
pub fn install_global(pool: Arc<EgressPool>) {
    let _ = GLOBAL.set(pool);
}

/// The installed pool, if any.
pub fn global() -> Option<Arc<EgressPool>> {
    GLOBAL.get().cloned()
}

const BURN_COOLDOWN: Duration = Duration::from_secs(600);
const AUTH_BAN: Duration = Duration::from_secs(86_400); // 24h: wrong creds don't heal fast
const MIN_INTERVAL: Duration = Duration::from_millis(1200);
const JITTER_MS: u64 = 1300;
const DIRECT_MIN_INTERVAL: Duration = Duration::from_millis(2500);
const DIRECT_JITTER_MS: u64 = 2000;
const HEALTH_DISK_VERSION: u32 = 1;

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

/// On-disk pair row: engine, egress id, health, remaining burn secs.
type DiskPair = (String, String, String, u64);
/// On-disk dead row: egress id, remaining bench secs.
type DiskDead = (String, u64);

#[derive(serde::Serialize, serde::Deserialize)]
struct EgressHealthDisk {
    #[serde(default = "disk_version")]
    version: u32,
    #[serde(default)]
    pairs: Vec<DiskPair>,
    #[serde(default)]
    dead: Vec<DiskDead>,
}

fn disk_version() -> u32 {
    HEALTH_DISK_VERSION
}

fn health_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("egress-health.json"))
}

fn persist_enabled() -> bool {
    if crate::config::cfg().state.no_disk_state {
        return false;
    }
    crate::config::cfg().proxy.egress_persist
}

fn health_to_str(h: Health) -> &'static str {
    match h {
        Health::Healthy => "healthy",
        Health::Suspect => "suspect",
        Health::Burned => "burned",
    }
}

fn health_from_str(s: &str) -> Health {
    match s {
        "healthy" => Health::Healthy,
        "burned" => Health::Burned,
        _ => Health::Suspect,
    }
}

fn remaining_secs(until: Option<Instant>) -> u64 {
    match until {
        None => 0,
        Some(t) => {
            let now = Instant::now();
            if t <= now {
                0
            } else {
                t.saturating_duration_since(now).as_secs()
            }
        }
    }
}

pub struct EgressPool {
    egresses: Vec<Egress>,
    pacing: Mutex<HashMap<(String, String), PaceGate>>,
    /// (engine|host, egress_id) -> state
    pairs: Mutex<HashMap<(String, String), PairState>>,
    /// Global proxy liveness (connect failures burn a proxy
    /// for ALL engines; a dead line is a dead line).
    dead: Mutex<HashMap<String, Instant>>,
    /// Stress gauge: consecutive-ish outcome EWMA
    /// (scaled x1000: 0 = all good, 1000 = everything fails).
    stress_ok: AtomicU32,
    stress_fail: AtomicU32,
    /// Set on pair/dead mutation; save swaps it off so an idle
    /// process never rewrites the file.
    health_dirty: AtomicBool,
    /// Per-lane RTT EWMA in milliseconds (search probes, fetch,
    /// crawl). Feeds pacing and doctor --deep slow-lane rows.
    rtt: Mutex<HashMap<String, f64>>,
    /// Fetch stickiness: host -> egress_id. One host rides one
    /// exit until a rotate signal (429/407/dead/timeout).
    sticky: Mutex<HashMap<String, String>>,
    /// Persona exclusive bind: egress_id -> host. A lane bound
    /// to persona A is never minted for persona B (no shared
    /// exit identity across domains).
    persona_lanes: Mutex<HashMap<String, String>>,
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
        let pool = Self {
            egresses,
            pacing: Mutex::new(HashMap::new()),
            pairs: Mutex::new(HashMap::new()),
            dead: Mutex::new(HashMap::new()),
            stress_ok: AtomicU32::new(2000), // seed optimistic
            stress_fail: AtomicU32::new(0),
            health_dirty: AtomicBool::new(false),
            rtt: Mutex::new(HashMap::new()),
            sticky: Mutex::new(HashMap::new()),
            persona_lanes: Mutex::new(HashMap::new()),
        };
        pool.load_health_disk();
        pool
    }

    pub fn from_env() -> Self {
        Self::new(crate::transport::proxy::load_all())
    }

    /// Load persisted (engine,egress) health and dead-lane benches so a
    /// restart never re-learns a burned proxy from zero.
    fn load_health_disk(&self) {
        if !persist_enabled() {
            return;
        }
        let Some(path) = health_path() else { return };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(disk) = serde_json::from_str::<EgressHealthDisk>(&raw) else {
            return;
        };
        if disk.version != HEALTH_DISK_VERSION {
            return;
        }
        let now = Instant::now();
        let mut pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (engine, egress, health, remaining) in disk.pairs {
            if remaining == 0 && health_from_str(&health) == Health::Burned {
                // Expired while the process was down: probation, not burned.
                pairs.insert(
                    (engine, egress),
                    PairState {
                        health: Health::Suspect,
                        burned_until: None,
                    },
                );
                continue;
            }
            pairs.insert(
                (engine, egress),
                PairState {
                    health: health_from_str(&health),
                    burned_until: if remaining > 0 {
                        Some(now + Duration::from_secs(remaining.min(86_400)))
                    } else {
                        None
                    },
                },
            );
        }
        drop(pairs);
        let mut dead = self
            .dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, remaining) in disk.dead {
            if remaining > 0 {
                dead.insert(id, now + Duration::from_secs(remaining.min(86_400)));
            }
        }
    }

    /// Atomic save of pair + dead health. Skipped when persistence is
    /// off or nothing mutated since the last save.
    fn save_health_disk_if_dirty(&self) {
        if !self.health_dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        if !persist_enabled() {
            return;
        }
        let Some(path) = health_path() else { return };
        let pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dead = self
            .dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let disk = EgressHealthDisk {
            version: HEALTH_DISK_VERSION,
            pairs: pairs
                .iter()
                .map(|((engine, egress), s)| {
                    (
                        engine.clone(),
                        egress.clone(),
                        health_to_str(s.health).to_string(),
                        remaining_secs(s.burned_until),
                    )
                })
                .collect(),
            dead: dead
                .iter()
                .map(|(id, until)| (id.clone(), remaining_secs(Some(*until))))
                .filter(|(_, rem)| *rem > 0)
                .collect(),
        };
        drop(pairs);
        drop(dead);
        let Ok(json) = serde_json::to_string(&disk) else {
            return;
        };
        if std::fs::create_dir_all(crate::paths::cache_dir()).is_err() {
            return;
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn mark_dirty(&self) {
        self.health_dirty.store(true, Ordering::Relaxed);
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
        {
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
        self.mark_dirty();
        self.save_health_disk_if_dirty();
    }

    /// Engine rejected us (429 / challenge / empty parse):
    /// burn the pair, not the engine.
    pub fn report_blocked(&self, engine: &str, egress_id: &str) {
        let engine = health_key(engine);
        self.stress_record(false);
        {
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
        self.mark_dirty();
        self.save_health_disk_if_dirty();
    }

    /// The egress line itself is dead (connect failure).
    pub fn report_dead(&self, egress_id: &str) {
        self.stress_record(false);
        if egress_id == "direct" {
            return; // direct failure = network down; don't mark
        }
        self.dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(egress_id.to_string(), Instant::now() + BURN_COOLDOWN);
        self.mark_dirty();
        self.save_health_disk_if_dirty();
    }

    /// Preflight guard: un-bench every lane (used when the
    /// probe endpoint itself died and burned all proxies).
    pub fn revive_all(&self) {
        self.dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.mark_dirty();
        self.save_health_disk_if_dirty();
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(egress_id.to_string(), Instant::now() + AUTH_BAN);
        self.mark_dirty();
        self.save_health_disk_if_dirty();
    }

    /// True when the lane is currently benched (dead or auth-banned).
    pub fn is_dead(&self, egress_id: &str) -> bool {
        self.dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(egress_id)
            .is_some_and(|&t| t > Instant::now())
    }

    /// True when (scope, lane) is burned and still cooling.
    fn pair_burned(&self, scope: &str, egress_id: &str) -> bool {
        let pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(
            pairs.get(&(scope.to_string(), egress_id.to_string())),
            Some(s) if s.health == Health::Burned
                && s.burned_until.is_some_and(|t| t > Instant::now())
        )
    }

    /// True when (scope, lane) is in post-block probation (first 429
    /// class signal). Fetch rotation deprioritizes these so a sticky
    /// host actually moves to another exit.
    fn pair_suspect(&self, scope: &str, egress_id: &str) -> bool {
        let pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(
            pairs.get(&(scope.to_string(), egress_id.to_string())),
            Some(s) if s.health == Health::Suspect
        )
    }

    /// Record a per-lane RTT sample (EWMA, alpha 0.25).
    /// Direct is included: the home IP can also go slow.
    pub fn observe_rtt(&self, egress_id: &str, rtt: Duration) {
        let ms = rtt.as_secs_f64() * 1000.0;
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let mut rtt = self
            .rtt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = rtt.entry(egress_id.to_string()).or_insert(ms);
        *e = *e * 0.75 + ms * 0.25;
    }

    /// Per-lane RTT EWMA in milliseconds, if any sample landed.
    pub fn rtt_ms(&self, egress_id: &str) -> Option<f64> {
        self.rtt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(egress_id)
            .copied()
    }

    /// Slow-lane threshold used by doctor and pacing (ms).
    pub fn slow_rtt_ms() -> f64 {
        2_500.0
    }

    /// One-line-per-lane summary for `doctor --deep` / status.
    /// Local-only: health + RTT + persona bind, no network.
    pub fn lane_summary(&self) -> Vec<LaneSummary> {
        let now = Instant::now();
        let pairs = self
            .pairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dead = self
            .dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rtt = self
            .rtt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let persona = self
            .persona_lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.egresses
            .iter()
            .map(|e| {
                let dead_until = dead.get(&e.id).copied().filter(|&t| t > now);
                let kind = if dead_until.is_some() {
                    let remaining = dead_until
                        .map(|t| t.saturating_duration_since(now))
                        .unwrap_or_default();
                    if remaining > Duration::from_secs(3600) {
                        "auth".to_string()
                    } else {
                        "dead".to_string()
                    }
                } else {
                    // Worst pair health across scopes that mention this lane.
                    let mut worst = "ok";
                    for ((_, id), s) in pairs.iter() {
                        if id != &e.id {
                            continue;
                        }
                        let label = match s.health {
                            Health::Healthy => "ok",
                            Health::Suspect => "suspect",
                            Health::Burned => {
                                if s.burned_until.is_some_and(|t| t > now) {
                                    "burned"
                                } else {
                                    "probation"
                                }
                            }
                        };
                        if rank(label) > rank(worst) {
                            worst = label;
                        }
                    }
                    match rtt.get(&e.id) {
                        Some(&ms) if ms >= Self::slow_rtt_ms() && worst == "ok" => "slow",
                        _ => worst,
                    }
                    .to_string()
                };
                LaneSummary {
                    id: e.id.clone(),
                    is_direct: e.proxy.is_none(),
                    state: kind,
                    rtt_ms: rtt.get(&e.id).map(|v| *v as u32),
                    persona_host: persona.get(&e.id).cloned(),
                }
            })
            .collect()
    }

    /// Sticky fetch lane for a host. First call picks and
    /// remembers; later calls return the same exit while it
    /// stays alive and unburned for this host. Among equally-clean
    /// lanes the lowest measured RTT wins. Direct is last
    /// resort only (protect the home IP).
    pub fn pick_fetch(&self, host: &str, direct_ok: bool) -> Option<Egress> {
        if host.is_empty() {
            return None;
        }
        let sticky_id = self
            .sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(host)
            .cloned();
        if let Some(id) = sticky_id {
            if !self.is_dead(&id)
                && !self.pair_burned(host, &id)
                && !self.pair_suspect(host, &id)
                && self.persona_lane_ok(host, &id)
                && let Some(eg) = self.egresses.iter().find(|e| e.id == id)
            {
                return Some(eg.clone());
            }
            self.sticky
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(host);
        }
        // Prefer a healthy, non-dead proxy that is not burned for
        // this host and not exclusively bound to another persona.
        // Unknown RTT counts as healthy (optimistic, matches pick());
        // among equally-ranked lanes the lowest measured RTT wins, so a
        // clean-but-slow lane in file order can never shadow a fast one
        // (first-healthy-wins made lane 1 the exit for every new host).
        let mut best: Option<&Egress> = None;
        let mut best_score = 0u8;
        let mut best_rtt = f64::MAX;
        for e in &self.egresses {
            if e.proxy.is_none() {
                continue;
            }
            if self.is_dead(&e.id) || self.pair_burned(host, &e.id) {
                continue;
            }
            if !self.persona_lane_ok(host, &e.id) {
                continue;
            }
            // 0 = this host's pair is in probation after a block:
            // never win against a clean lane (rotation must move).
            let score = if self.pair_suspect(host, &e.id) {
                0
            } else if self
                .rtt_ms(&e.id)
                .is_some_and(|ms| ms >= Self::slow_rtt_ms())
            {
                1
            } else {
                2
            };
            let rtt = self.rtt_ms(&e.id).unwrap_or(f64::MAX);
            if score > best_score || (best.is_some() && score == best_score && rtt < best_rtt) {
                best = Some(e);
                best_score = score;
                best_rtt = rtt;
            }
        }
        if let Some(e) = best {
            self.sticky
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(host.to_string(), e.id.clone());
            return Some(e.clone());
        }
        if direct_ok {
            return self.egresses.first().cloned();
        }
        None
    }

    /// True when the host's sticky lane is a proxy (not direct).
    /// Read-only: does not assign a lane.
    pub fn sticky_is_proxy(&self, host: &str) -> bool {
        let Some(id) = self
            .sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(host)
            .cloned()
        else {
            return false;
        };
        id != "direct"
    }

    /// Drop the host's sticky lane so the next pick rotates.
    /// Does not change health: rotation is the signal.
    pub fn rotate_fetch(&self, host: &str) {
        self.sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(host);
    }

    /// 429 / challenge on this host+lane: burn the pair and rotate.
    pub fn note_fetch_rate_limited(&self, host: &str, egress_id: &str) {
        self.report_blocked(host, egress_id);
        self.rotate_fetch(host);
    }

    /// Soft timeout / slow: pair-scoped probation + drop stickiness.
    /// No global bench (the line may be fine for other hosts).
    pub fn note_fetch_timeout(&self, host: &str, egress_id: &str) {
        self.report_blocked(host, egress_id);
        self.rotate_fetch(host);
    }

    /// Origin-side TLS failure (certificate verify) on the fetch
    /// path: pair-scoped probation + rotation, never a global bench.
    /// The failed certificate is the far side's, so the lane stays
    /// healthy for other hosts; the host moves anyway, because a
    /// lane that intercepts TLS re-signs every host it carries and a
    /// sticky host would otherwise retry the same lane forever.
    pub fn note_fetch_origin_tls(&self, host: &str, egress_id: &str) {
        self.report_blocked(host, egress_id);
        self.rotate_fetch(host);
    }

    /// CONNECT-dead or auth on the fetch path: global bench + rotate.
    pub fn note_fetch_dead(&self, host: &str, egress_id: &str) {
        self.report_dead(egress_id);
        self.rotate_fetch(host);
    }

    pub fn note_fetch_auth_fail(&self, host: &str, egress_id: &str) {
        self.report_auth_fail(egress_id);
        self.rotate_fetch(host);
    }

    /// Exclusive persona bind: lane → host. A second host cannot
    /// claim a lane already bound.
    pub fn bind_persona_lane(&self, host: &str, egress_id: &str) {
        if egress_id == "direct" || host.is_empty() {
            return;
        }
        self.persona_lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(egress_id.to_string(), host.to_string());
        self.sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host.to_string(), egress_id.to_string());
    }

    /// Free every lane bound to this host (quarantine / remint).
    pub fn release_persona_lane(&self, host: &str) {
        self.persona_lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, h| h != host);
        self.sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(host);
    }

    /// False when the lane is exclusively bound to a different host.
    pub fn persona_lane_ok(&self, host: &str, egress_id: &str) -> bool {
        match self
            .persona_lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(egress_id)
        {
            None => true,
            Some(h) => h == host,
        }
    }

    /// Pick a lane for a new persona mint: alive, not burned for
    /// this host, not bound to a foreign persona.
    pub fn pick_persona_lane(&self, host: &str) -> Option<Egress> {
        if host.is_empty() || !self.has_proxies() {
            return None;
        }
        // Reuse this host's existing bind when still healthy.
        let bound = self
            .sticky
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(host)
            .cloned();
        if let Some(id) = bound
            && !self.is_dead(&id)
            && !self.pair_burned(host, &id)
            && self.persona_lane_ok(host, &id)
            && let Some(eg) = self.egresses.iter().find(|e| e.id == id)
        {
            return Some(eg.clone());
        }
        for e in &self.egresses {
            if e.proxy.is_none()
                || self.is_dead(&e.id)
                || self.pair_burned(host, &e.id)
                || !self.persona_lane_ok(host, &e.id)
            {
                continue;
            }
            return Some(e.clone());
        }
        None
    }

    /// Pacing with jitter: this (engine, egress) pair is
    /// not hit more than once per randomized interval.
    /// The premium lane paces slower : protect the home IP.
    /// Slow lanes (high RTT EWMA) get a small extra gap.
    pub async fn pace(&self, engine: &str, egress_id: &str) {
        let (base, jit) = if egress_id == "direct" {
            (DIRECT_MIN_INTERVAL, DIRECT_JITTER_MS)
        } else {
            (MIN_INTERVAL, JITTER_MS)
        };
        let slow_extra = match self.rtt_ms(egress_id) {
            Some(ms) if ms >= Self::slow_rtt_ms() => {
                Duration::from_millis(((ms - Self::slow_rtt_ms()) / 8.0).min(400.0) as u64)
            }
            _ => Duration::ZERO,
        };
        let interval = base + Duration::from_millis(jitter(jit)) + slow_extra;
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

/// Local-only lane row for doctor --deep / status.
#[derive(Debug, Clone)]
pub struct LaneSummary {
    pub id: String,
    pub is_direct: bool,
    /// ok | slow | suspect | probation | burned | dead | auth
    pub state: String,
    pub rtt_ms: Option<u32>,
    pub persona_host: Option<String>,
}

fn rank(state: &str) -> u8 {
    match state {
        "ok" => 0,
        "slow" => 1,
        "probation" => 2,
        "suspect" => 3,
        "burned" => 4,
        "dead" | "auth" => 5,
        _ => 0,
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

    /// A burned proxy pair must survive process restart: drop the pool,
    /// rebuild from the same cache dir, and the next pick must skip that
    /// lane (not re-learn the burn from zero).
    #[test]
    fn burned_pair_survives_restart() {
        let dir = std::env::temp_dir().join(format!(
            "donsetch-egress-persist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Isolate: cache_dir() reads this env per call.
        // SAFETY: test-only mutation of the process env; nextest runs
        // each test in its own process.
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        let proxy = Proxy::parse("http://127.0.0.1:23456").unwrap();
        let id = proxy.id();
        {
            let pool = EgressPool::new(vec![proxy.clone()]);
            pool.report_blocked("bing", &id);
            pool.report_blocked("bing", &id);
            assert!(pool.pick("bing", &[], false).is_none());
        }
        // New process shape: fresh pool, same cache.
        let pool2 = EgressPool::new(vec![proxy.clone()]);
        assert!(
            pool2.pick("bing", &[], false).is_none(),
            "burned (bing, proxy) pair must survive restart"
        );
        // Other engines still work on that lane after the restart.
        assert_eq!(
            pool2.pick("yahoo", &[], false).map(|e| e.id),
            Some(id.clone())
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    /// DONSETCH_NO_EGRESS_PERSIST must map to the typed knob and prevent
    /// egress-health.json writes. nextest is process-per-test: env is set
    /// before the first cfg() touch in this process.
    #[test]
    fn persist_kill_switch_writes_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "donsetch-egress-nopersist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
            std::env::set_var("DONSETCH_NO_EGRESS_PERSIST", "1");
        }
        assert!(
            !crate::config::cfg().proxy.egress_persist,
            "DONSETCH_NO_EGRESS_PERSIST must set proxy.egress_persist=false"
        );
        let proxy = Proxy::parse("http://127.0.0.1:23457").unwrap();
        let pool = EgressPool::new(vec![proxy.clone()]);
        pool.report_blocked("bing", &proxy.id());
        pool.report_blocked("bing", &proxy.id());
        assert!(
            !dir.join("egress-health.json").exists(),
            "kill switch must not write egress-health.json"
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
            std::env::remove_var("DONSETCH_NO_EGRESS_PERSIST");
        }
    }

    // ── A2: fetch stickiness, rotation, persona bind, RTT ──
    //
    // Isolate the cache dir: EgressPool::new loads egress-health.json,
    // and a previous run's benches for these loopback ports would make
    // pick_fetch return None. nextest is process-per-test; the env is
    // set before the first cfg()/pool touch.

    fn isolate_cache(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "donsetch-egress-a2-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        dir
    }

    #[test]
    fn pick_fetch_is_sticky_until_rotate() {
        let dir = isolate_cache("sticky");
        let p1 = Proxy::parse("http://127.0.0.1:24001").unwrap();
        let p2 = Proxy::parse("http://127.0.0.1:24002").unwrap();
        let pool = EgressPool::new(vec![p1, p2]);
        let a = pool.pick_fetch("example.com", false).expect("proxy lane");
        let b = pool.pick_fetch("example.com", false).expect("proxy lane");
        assert_eq!(a.id, b.id, "same host must stick to the same lane");
        pool.note_fetch_rate_limited("example.com", &a.id);
        let c = pool.pick_fetch("example.com", false).expect("rotated");
        assert_ne!(
            c.id, a.id,
            "429 must rotate off the burned lane (got the same one again)"
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    #[test]
    fn pick_fetch_skips_dead_lane() {
        let dir = isolate_cache("dead");
        let p1 = Proxy::parse("http://127.0.0.1:24011").unwrap();
        let p2 = Proxy::parse("http://127.0.0.1:24012").unwrap();
        let id1 = p1.id();
        let pool = EgressPool::new(vec![p1, p2]);
        pool.report_dead(&id1);
        let eg = pool.pick_fetch("example.com", false).expect("live lane");
        assert_ne!(eg.id, id1, "dead lane must never be assigned to fetch");
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    #[test]
    fn pick_fetch_prefers_the_lowest_rtt_clean_lane() {
        let dir = isolate_cache("rtt-order");
        let p1 = Proxy::parse("http://127.0.0.1:24021").unwrap();
        let p2 = Proxy::parse("http://127.0.0.1:24022").unwrap();
        let (id1, id2) = (p1.id(), p2.id());
        let pool = EgressPool::new(vec![p1, p2]);
        // Both lanes clean (under the slow threshold); lane 1 is slower.
        pool.observe_rtt(&id1, std::time::Duration::from_millis(900));
        pool.observe_rtt(&id2, std::time::Duration::from_millis(120));
        let lane = pool.pick_fetch("example.com", false).expect("lane");
        assert_eq!(
            lane.id, id2,
            "the faster clean lane must win; file order is only a tie-break"
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    #[test]
    fn persona_lane_is_exclusive_across_hosts() {
        let dir = isolate_cache("persona-ex");
        let p1 = Proxy::parse("http://127.0.0.1:24021").unwrap();
        let p2 = Proxy::parse("http://127.0.0.1:24022").unwrap();
        let id1 = p1.id();
        let id2 = p2.id();
        let pool = EgressPool::new(vec![p1, p2]);
        pool.bind_persona_lane("a.example", &id1);
        assert!(!pool.persona_lane_ok("b.example", &id1));
        assert!(pool.persona_lane_ok("a.example", &id1));
        let for_b = pool.pick_persona_lane("b.example").expect("other lane");
        assert_eq!(for_b.id, id2, "foreign-persona lane must not be reused");
        pool.release_persona_lane("a.example");
        assert!(
            pool.persona_lane_ok("b.example", &id1),
            "release frees the bind"
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    #[test]
    fn persona_mint_never_takes_burned_lane() {
        let dir = isolate_cache("persona-burn");
        let p1 = Proxy::parse("http://127.0.0.1:24031").unwrap();
        let id1 = p1.id();
        let pool = EgressPool::new(vec![p1]);
        pool.report_blocked("a.example", &id1);
        pool.report_blocked("a.example", &id1);
        assert!(
            pool.pick_persona_lane("a.example").is_none(),
            "burned pair must not mint a persona on that lane"
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    #[test]
    fn rtt_ewma_moves_and_lane_summary_flags_slow() {
        let dir = isolate_cache("rtt");
        let p = Proxy::parse("http://127.0.0.1:24041").unwrap();
        let id = p.id();
        let pool = EgressPool::new(vec![p]);
        pool.observe_rtt(&id, Duration::from_millis(3000));
        pool.observe_rtt(&id, Duration::from_millis(3000));
        let rtt = pool.rtt_ms(&id).expect("rtt recorded");
        assert!(rtt >= 2500.0, "slow sample must land in EWMA, got {rtt}");
        let summary = pool.lane_summary();
        let row = summary.iter().find(|s| s.id == id).expect("lane row");
        assert_eq!(row.state, "slow", "lane_summary must flag slow lanes");
        assert!(row.rtt_ms.is_some());
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    #[test]
    fn note_fetch_timeout_rotates_without_global_bench() {
        let dir = isolate_cache("timeout");
        let p1 = Proxy::parse("http://127.0.0.1:24051").unwrap();
        let p2 = Proxy::parse("http://127.0.0.1:24052").unwrap();
        let pool = EgressPool::new(vec![p1, p2]);
        let first = pool.pick_fetch("example.com", false).unwrap();
        pool.note_fetch_timeout("example.com", &first.id);
        assert!(
            !pool.is_dead(&first.id),
            "timeout must not globally bench the lane"
        );
        let next = pool.pick_fetch("example.com", false).unwrap();
        assert_ne!(next.id, first.id, "timeout must rotate stickiness");
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }
}
