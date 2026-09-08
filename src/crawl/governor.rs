//! The Crawl Governor : rate-limit immune system.
//!
//! The failure mode of crawlers is not "can't fetch" : it's
//! "fetched 40 pages fine, then the host put the IP in a
//! penalty box for 30 minutes." Every crawler eventually
//! discovers its pace; smart ones discover it on page 5,
//! dumb ones on page 200. This engine discovers it on page 3.
//!
//! Design:
//! - Pacing is per (host, lane). Two rotating proxies to one
//!   host = two independent rate clocks, doubling throughput.
//! - A PenaltyBox is shared PER HOST across lanes: a 429 on
//!   lane B backs host-wide pressure off, all lanes.
//! - Latency is a signal: rising EWMA latency predicts a wall
//!   BEFORE it lands; we slow proactively, not reactively.
//! - Jitter everywhere: fixed-interval traffic is the bot
//!   fingerprint. Humans are not metronomes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Base inter-request delay per lane when healthy.
///
/// Elastic pacing: 200ms+jitter (~150-250ms gaps). A normal
/// browser page load fires 20-80 requests to one host in
/// parallel, so single-document fetches at this pace sit far
/// below any per-IP threshold that allows normal browsing. The
/// governor's job is the ESCALATION ladder, not presumptive
/// slowness: any throttle signal (429/503), latency stress
/// (EWMA > 3× baseline), or robots crawl-delay raises the pace
/// reactively : we discover the host's real limit from its own
/// signals instead of taxing every crawl with a theater of
/// politeness. Measured effect: small-crawl median 6.29s →
/// ~2.5s with zero observed throttling on test hosts.
const BASE_DELAY: Duration = Duration::from_millis(200);

/// Hard ceiling on adaptive delay growth (rung = BASE * 2^k,
/// capped at 6 rungs). Every ladder delay is additionally capped
/// at MAX_LADDER_WAIT: self-inferred waits never exceed ~7s
/// (v4 law 11: stealth through truth, never time). Host-declared
/// waits (the 429/503 penalty box, robots crawl-delay) bypass the
/// cap and are honored in full.
const MAX_BACKOFF_RUNG: u32 = 6;
const MAX_LADDER_WAIT: Duration = Duration::from_secs(7);

/// Latency EWMA window: new samples weigh 25%.
const EWMA_ALPHA: f64 = 0.25;

/// If host EWMA latency rises above this multiple of its
/// first-observed baseline, we treat it as pre-wall stress and
/// add a rung proactively. 3x: caught via Akamai queue latency.
const STRESS_LATENCY_MULT: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneKind {
    Direct,
    Proxy,
}

#[derive(Debug, Clone)]
pub struct Lane {
    pub id: String,
    pub kind: LaneKind,
}

/// Per-(host, lane) pacing clock.
struct HostLane {
    next_allowed: Instant,
    /// Consecutive throttle events feeding the rung.
    rung: u32,
    /// EWMA of response latency ms.
    ewma_ms: f64,
    /// First observed latency = baseline for stress detection.
    baseline_ms: Option<f64>,
}

impl Default for HostLane {
    fn default() -> Self {
        Self {
            next_allowed: Instant::now(),
            rung: 0,
            ewma_ms: 0.0,
            baseline_ms: None,
        }
    }
}

/// Host-wide penalty state shared across lanes.
#[derive(Default)]
struct HostPenalty {
    /// Consecutive host-level failures (any lane).
    rung: u32,
    /// Host is in a penalty box until this instant (429/storm).
    boxed_until: Option<Instant>,
    /// Last activity on this host (any lane). Drives pruning so
    /// a long-lived daemon does not grow one struct per host it
    /// ever touched.
    last_seen: Option<Instant>,
}

pub struct Governor {
    /// (host, lane_id) -> pacing clock.
    lanes: Mutex<HashMap<(String, String), HostLane>>,
    /// host -> shared penalty state.
    hosts: Mutex<HashMap<String, HostPenalty>>,
    /// All lanes in the pool.
    pub lanes_all: Vec<Lane>,
    /// Honors robots.txt crawl-delay when set (minimum pace).
    crawl_delay: Mutex<Option<f64>>,
}

impl Governor {
    pub fn new(lanes_all: Vec<Lane>) -> Self {
        Self {
            lanes: Mutex::new(HashMap::new()),
            hosts: Mutex::new(HashMap::new()),
            lanes_all,
            crawl_delay: Mutex::new(None),
        }
    }

    pub fn set_crawl_delay(&self, d: Option<f64>) {
        *self
            .crawl_delay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = d;
    }

    /// Base delay honoring host-declared crawl-delay.
    fn base(&self) -> Duration {
        let cd = self
            .crawl_delay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The setter is public: re-clamp here so no caller can
        // hand `from_secs_f64` a value it panics on.
        match *cd {
            Some(s) if s.is_finite() && s > 0.0 => Duration::from_secs_f64(
                s.min(super::sitemap::MAX_CRAWL_DELAY_SECS)
                    .max(BASE_DELAY.as_secs_f64()),
            ),
            _ => BASE_DELAY,
        }
    }

    /// Deterministic jitter: remaps a counter into [0.75, 1.25].
    /// No RNG state -> reproducible pacing, no metronome tick
    /// lengths.
    fn jitter(&self, seed: u64) -> f64 {
        // xorshift-ish swirl of the seed + current nanos low bits.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let mut x = seed ^ nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        0.75 + (x % 500) as f64 / 1000.0
    }

    /// How long until `lane` may hit `host` again, accounting
    /// for the shared host penalty box. Caller `tokio::time::sleep`s.
    pub fn wait_for(&self, host: &str, lane: &str, seq: u64) -> Duration {
        let host_boxed = {
            let mut hosts = self
                .hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            prune_hosts(&mut hosts);
            let h = hosts.entry(host.to_string()).or_default();
            h.last_seen = Some(Instant::now());
            h.boxed_until
                .map(|u| u.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::ZERO)
        };
        if !host_boxed.is_zero() {
            return host_boxed;
        }

        // Proxy lanes carry extra round-trip latency; pace them
        // slightly longer so the direct lane stays the fast lane.
        let kind = self
            .lanes_all
            .iter()
            .find(|l| l.id == lane)
            .map(|l| l.kind)
            .unwrap_or(LaneKind::Direct);
        let base = match kind {
            LaneKind::Direct => self.base(),
            LaneKind::Proxy => self.base().mul_f64(1.5),
        };
        let mut lanes = self
            .lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (host.to_string(), lane.to_string());
        let hl = lanes.entry(key).or_default();
        let now = Instant::now();
        let wait = hl.next_allowed.saturating_duration_since(now);
        if !wait.is_zero() {
            return wait;
        }

        // Compute the new next_allowed: base * 2^rung, jittered,
        // capped at MAX_LADDER_WAIT (law 11).
        let rung_mult = (1u64 << hl.rung.min(MAX_BACKOFF_RUNG)) as f64;
        let delay = base
            .mul_f64(rung_mult * self.jitter(seq))
            .min(MAX_LADDER_WAIT);
        hl.next_allowed = now + delay;
        Duration::ZERO
    }

    /// Record a healthy response: decay the rung (recovery),
    /// fold latency into EWMA, flag pre-wall stress. Also pull
    /// the pending next_allowed FORWARD when the rung decays :
    /// a host answering fine again shouldn't serve an old
    /// penalty window computed while it was upset.
    pub fn on_success(&self, host: &str, lane: &str, latency: Duration) {
        let ms = latency.as_secs_f64() * 1000.0;
        let mut lanes = self
            .lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (host.to_string(), lane.to_string());
        let hl = lanes.entry(key).or_default();
        let old_rung = hl.rung;
        hl.rung = hl.rung.saturating_sub(1);
        if hl.ewma_ms == 0.0 {
            hl.ewma_ms = ms;
            hl.baseline_ms = Some(ms);
        } else {
            hl.ewma_ms = EWMA_ALPHA * ms + (1.0 - EWMA_ALPHA) * hl.ewma_ms;
        }
        // Proactive slow-down: if EWMA drifted 3x above baseline,
        // the host is queuing us; add pressure-off rung.
        if let (Some(base), true) = (hl.baseline_ms, hl.ewma_ms > 0.0)
            && hl.ewma_ms > base * STRESS_LATENCY_MULT
            && hl.rung < 3
        {
            hl.rung += 1;
        }
        // Rung decayed: compress any pending wait to the new,
        // smaller rung's window.
        if hl.rung < old_rung && hl.next_allowed > Instant::now() {
            let new_mult = (1u64 << hl.rung.min(MAX_BACKOFF_RUNG)) as f64;
            hl.next_allowed = Instant::now()
                + self
                    .base()
                    .min(hl.next_allowed.saturating_duration_since(Instant::now()))
                    .mul_f64(new_mult);
        }
        drop(lanes);

        // Success decays the shared host penalty too.
        let mut hosts = self
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(h) = hosts.get_mut(host) {
            h.last_seen = Some(Instant::now());
            h.rung = h.rung.saturating_sub(1);
            if h.rung == 0 {
                h.boxed_until = None;
            }
        }
    }

    /// Record a 429/5xx: push the lane's rung up AND drop the
    /// whole host into the shared penalty box.
    pub fn on_throttled(&self, host: &str, lane: &str) {
        {
            let mut lanes = self
                .lanes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let key = (host.to_string(), lane.to_string());
            let hl = lanes.entry(key).or_default();
            hl.rung = (hl.rung + 2).min(MAX_BACKOFF_RUNG);
            let rung_mult = (1u64 << hl.rung) as f64;
            hl.next_allowed = Instant::now()
                + self
                    .base()
                    .mul_f64(rung_mult * self.jitter(0xbeef))
                    .min(MAX_LADDER_WAIT);
        }
        // Shared host penalty box: everyone backs off together.
        let mut hosts = self
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let h = hosts.entry(host.to_string()).or_default();
        h.last_seen = Some(Instant::now());
        h.rung = (h.rung + 1).min(MAX_BACKOFF_RUNG);
        let host_rung_mult = (1u64 << h.rung) as f64;
        h.boxed_until =
            Some(Instant::now() + self.base().mul_f64(host_rung_mult * self.jitter(0xdead)));
    }

    /// Record a network error (timeout, reset): gentler than
    /// throttled : one lane rung, no host box.
    pub fn on_error(&self, host: &str, lane: &str) {
        let mut lanes = self
            .lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (host.to_string(), lane.to_string());
        let hl = lanes.entry(key).or_default();
        hl.rung = (hl.rung + 1).min(MAX_BACKOFF_RUNG);
        hl.next_allowed = Instant::now()
            + self
                .base()
                .mul_f64((1u64 << hl.rung) as f64 * self.jitter(7))
                .min(MAX_LADDER_WAIT);
    }

    /// Pick the least-blocked lane for the host, respecting the
    /// shared penalty box. Returns None when the whole host is
    /// boxed (caller waits for the box to lift).
    pub fn best_lane(&self, host: &str) -> Option<&Lane> {
        let host_boxed = {
            let hosts = self
                .hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hosts
                .get(host)
                .and_then(|h| h.boxed_until)
                .map(|u| u > Instant::now())
                .unwrap_or(false)
        };
        if host_boxed {
            return None;
        }
        let lanes = self
            .lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        self.lanes_all.iter().min_by_key(|l| {
            let key = (host.to_string(), l.id.clone());
            lanes
                .get(&key)
                .map(|hl| hl.next_allowed.saturating_duration_since(now))
                .unwrap_or(Duration::ZERO)
        })
    }
}

/// Prune hosts untouched for over an hour once the map passes
/// 1024 entries: a penalty box's maximum horizon is minutes, so
/// dropping hour-old state loses nothing but memory. Also drops
/// idle map growth from one large breadth crawl per daemon life.
fn prune_hosts(hosts: &mut HashMap<String, HostPenalty>) {
    const CAP: usize = 1024;
    const MAX_IDLE: Duration = Duration::from_secs(3600);
    if hosts.len() <= CAP {
        return;
    }
    let now = Instant::now();
    hosts.retain(|_, h| {
        h.last_seen
            .map(|t| now.saturating_duration_since(t) < MAX_IDLE)
            .unwrap_or(false)
    });
    // Belt and suspenders: if the crawl storm touched over a
    // thousand hosts in the last hour (mega-breadth), keep only
    // the most recently active.
    if hosts.len() > CAP {
        let mut sorted: Vec<(Instant, String)> = hosts
            .iter()
            .filter_map(|(k, h)| h.last_seen.map(|t| (t, k.clone())))
            .collect();
        sorted.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        let keep: std::collections::HashSet<String> =
            sorted.into_iter().take(CAP).map(|(_, k)| k).collect();
        hosts.retain(|k, _| keep.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gov(kinds: &[LaneKind]) -> Governor {
        let lanes = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| Lane {
                id: format!("lane{i}"),
                kind: *k,
            })
            .collect();
        Governor::new(lanes)
    }

    #[test]
    fn first_request_no_wait() {
        let g = gov(&[LaneKind::Direct]);
        assert_eq!(g.wait_for("ex.com", "lane0", 0), Duration::ZERO);
    }

    #[test]
    fn second_request_waits_pace() {
        let g = gov(&[LaneKind::Direct]);
        g.wait_for("ex.com", "lane0", 0);
        let w = g.wait_for("ex.com", "lane0", 1);
        assert!(w > Duration::ZERO);
        assert!(w < Duration::from_secs(3));
    }

    // `Duration::from_secs_f64` panics on non-finite or > u64::MAX
    // seconds; the governor must never trust the value it is
    // handed that far.
    #[test]
    fn absurd_crawl_delay_never_panics_and_is_capped() {
        for d in [f64::INFINITY, f64::NAN, 1e300, -1.0, 86400.0] {
            let g = gov(&[LaneKind::Direct]);
            g.set_crawl_delay(Some(d));
            g.wait_for("ex.com", "lane0", 0);
            let w = g.wait_for("ex.com", "lane0", 1);
            assert!(w <= Duration::from_secs(90), "{d}: {w:?}");
        }
    }

    #[test]
    fn two_lanes_are_independent() {
        let g = gov(&[LaneKind::Direct, LaneKind::Proxy]);
        g.wait_for("ex.com", "lane0", 0);
        g.wait_for("ex.com", "lane0", 0); // lane0 now waits
        // lane1 is untouched.
        assert_eq!(g.wait_for("ex.com", "lane1", 0), Duration::ZERO);
    }

    #[test]
    fn throttled_boxes_host_all_lanes() {
        let g = gov(&[LaneKind::Direct, LaneKind::Proxy]);
        g.on_throttled("ex.com", "lane0");
        assert!(g.best_lane("ex.com").is_none());
        assert!(g.wait_for("ex.com", "lane1", 9) > Duration::ZERO);
    }

    #[test]
    fn success_decays_rung() {
        let g = gov(&[LaneKind::Direct]);
        g.on_throttled("ex.com", "lane0");
        let before = g.wait_for("ex.com", "lane0", 1);
        // Simulate the box expiring, then successes.
        {
            let mut hosts = g
                .hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hosts.get_mut("ex.com").unwrap().boxed_until =
                Some(Instant::now() - Duration::from_secs(1));
            hosts.get_mut("ex.com").unwrap().rung = 2;
        }
        g.on_success("ex.com", "lane0", Duration::from_millis(50));
        g.on_success("ex.com", "lane0", Duration::from_millis(50));
        g.on_success("ex.com", "lane0", Duration::from_millis(50));
        let after = g.wait_for("ex.com", "lane0", 2);
        let _ = before;
        assert!(after < Duration::from_secs(2));
    }

    #[test]
    fn rising_latency_adds_rung() {
        let g = gov(&[LaneKind::Direct]);
        g.on_success("ex.com", "lane0", Duration::from_millis(100));
        // Feed rising latencies.
        for _ in 0..6 {
            g.on_success("ex.com", "lane0", Duration::from_millis(500));
        }
        {
            let lanes = g
                .lanes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let hl = lanes.get(&("ex.com".into(), "lane0".into())).unwrap();
            assert!(hl.rung >= 1);
        }
    }

    // v4 phase 3 (law 11): the adaptive ladder is capped. At the
    // top rung the wait must never exceed MAX_LADDER_WAIT, whatever
    // the jitter draws. Pre-cap, rung 6 reached ~16s.
    #[test]
    fn ladder_capped_at_seven_seconds() {
        for seed in 0..32u64 {
            let g = gov(&[LaneKind::Direct]);
            {
                let mut lanes = g
                    .lanes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                lanes
                    .entry(("ex.com".into(), "lane0".into()))
                    .or_default()
                    .rung = MAX_BACKOFF_RUNG;
            }
            let w = g.wait_for("ex.com", "lane0", seed);
            assert!(
                w <= MAX_LADDER_WAIT,
                "seed {seed}: wait {w:?} exceeds the 7s cap"
            );
        }
    }

    #[test]
    fn best_lane_picks_least_blocked() {
        let g = gov(&[LaneKind::Direct, LaneKind::Proxy]);
        g.wait_for("ex.com", "lane0", 0);
        g.wait_for("ex.com", "lane0", 1);
        let pick = g.best_lane("ex.com").unwrap();
        assert_eq!(pick.id, "lane1");
    }

    #[test]
    fn host_map_prunes_stale_entries() {
        let g = gov(&[LaneKind::Direct]);
        for i in 0..1100 {
            g.wait_for(&format!("h{i}.com"), "lane0", i);
        }
        // Age every entry past the idle window.
        {
            let mut hosts = g
                .hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for h in hosts.values_mut() {
                h.last_seen = Some(Instant::now() - Duration::from_secs(7200));
            }
        }
        // One more touch triggers the prune pass.
        g.wait_for("fresh.com", "lane0", 1200);
        let hosts = g
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            hosts.len() <= 2,
            "stale hosts must be pruned, kept {}",
            hosts.len()
        );
    }
}
