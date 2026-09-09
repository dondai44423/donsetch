//! GhostManager : the daemon's browser lifecycle brain.
//!
//! A pool of warm browsers, one tab per slot, one job per slot at
//! a time. Frozen between jobs (0 CPU), reaped after 10 min frozen,
//! crash-transparent on acquire. Slots are keyed by persona identity
//! (the profile value) with a host-affinity hint on acquire: a repeat
//! visit to the same host reuses the same-profile browser that
//! already has that site's session state warm. Default pool: 3 slots.
//! `DONSETCH_GHOST_POOL_SLOTS` sizes it (1-16); `DONSETCH_NO_GHOST_POOL`
//! forces the legacy single-slot behavior.
//!
//! Concurrency: each slot carries its own lock; a held browser job
//! pins exactly its slot, other slots stay free. Selection reads a
//! lightweight metadata snapshot under its own short lock, then
//! locks the chosen slot only.
//!
//! On Linux, an Xvfb virtual display is started at init and kept warm
//! for the whole pool. Ghost launches headful Chrome on this display :
//! the stealth path that passes Cloudflare/DataDome.

use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::{FREEZE_AFTER, Ghost, REAP_AFTER};
use crate::error::FetchError;
use crate::profile::BrowserProfile;

/// Pool size: default 3 warm slots. Env override
/// `DONSETCH_GHOST_POOL_SLOTS` clamps to 1..=16; 0 falls back to the
/// default (a zero-slot pool would disable the warm path entirely,
/// which the kill switch owns); `DONSETCH_NO_GHOST_POOL` forces the
/// legacy single-slot path.
fn pool_slots(default: usize) -> usize {
    pool_size(
        std::env::var_os("DONSETCH_NO_GHOST_POOL").is_some(),
        std::env::var("DONSETCH_GHOST_POOL_SLOTS").ok().as_deref(),
        default,
    )
}

/// Sizing rule: kill switch > explicit > default; explicit clamps
/// 1..=16; zero falls back to default (pools bounded to a real slot).
fn pool_size(no_pool: bool, slots_env: Option<&str>, default: usize) -> usize {
    if no_pool {
        return 1;
    }
    match slots_env.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(0) => default,
        Some(n) => n.clamp(1, 16),
        None => default,
    }
}

/// Metadata snapshot of one slot for the selector. Written at the
/// state-change points (launch, guard drop, reap, persona kill);
/// read once per acquire. Cheap Copy snapshot.
#[derive(Clone)]
struct Snap {
    live: bool,
    key: Option<u64>,
    host: Option<String>,
    used: Instant,
}

struct Slot {
    ghost: Option<Ghost>,
    /// Persona identity this slot was launched under (hash of the
    /// profile fields). Cleared on reap/kill; a fresh persona claims
    /// the slot by relaunching, never by inheriting a stranger's
    /// browser.
    key: Option<u64>,
    /// Host affinity hint: the host of the last acquire this slot
    /// served. A repeat hit on that host reuses the session state.
    host: Option<String>,
}

pub struct GhostManager {
    meta: Arc<Mutex<Vec<Snap>>>,
    slots: Vec<Arc<AsyncMutex<Slot>>>,
    /// Xvfb display string (":99") on Linux, None elsewhere. The
    /// display is pool-wide: every slot's Chrome attaches to it.
    display: Option<String>,
    /// The pool-wide Xvfb handle; killed once at daemon shutdown
    /// (previously one per manager; the pool shares one).
    xvfb: AsyncMutex<Option<super::xvfb::Xvfb>>,
}

/// RAII handle: derefs straight to the live Ghost of ONE slot, so
/// async ops hold only that slot's lock across awaits. The others
/// stay free. Drop stamps the slot's last_used in the meta snapshot.
pub struct GhostGuard {
    meta: Arc<Mutex<Vec<Snap>>>,
    guard: OwnedMutexGuard<Slot>,
    idx: usize,
}

impl Deref for GhostGuard {
    type Target = Ghost;
    fn deref(&self) -> &Ghost {
        self.guard.ghost.as_ref().expect("ghost in guard")
    }
}

impl DerefMut for GhostGuard {
    fn deref_mut(&mut self) -> &mut Ghost {
        self.guard.ghost.as_mut().expect("ghost in guard")
    }
}

impl Drop for GhostGuard {
    fn drop(&mut self) {
        // Stamp the slot we held. tokio's blocking_lock is safe in a
        // drop path (contended only across job lifetimes, the meta
        // critical section is microseconds).
        if let Ok(mut snaps) = self.meta.lock()
            && let Some(snap) = snaps.get_mut(self.idx)
        {
            snap.used = Instant::now();
        }
        // On Windows and macOS, a frozen browser window stays visible
        // (Windows: taskbar, macOS: desktop). On Linux with Xvfb the
        // window is on a virtual display (invisible), so the warm-browser
        // optimization is safe there. On Linux headless (no Xvfb), there is
        // no visible window either, so freezing is safe.
        //
        // Kill the browser on drop for Windows and macOS so no stuck,
        // unresponsive Chrome window lingers after a fetch. The Proc's
        // Drop closes the handle and the browser tree is reaped.
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            self.guard.ghost = None;
            if let Ok(mut snaps) = self.meta.lock()
                && let Some(snap) = snaps.get_mut(self.idx)
            {
                snap.live = false;
                snap.key = None;
                snap.host = None;
            }
        }
    }
}

/// The Xvfb install hint belongs to Linux-family systems only.
/// macOS and Windows run headful off-screen natively; printing
/// apt/pacman advice there was noise on every session start
/// (issue #81). A pure function so the platform gate is
/// unit-testable on the CI platforms.
fn xvfb_missing_hint() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        Some(
            "[ghost] Xvfb not found : install with `apt install xvfb` or `pacman -S xorg-server-xvfb` (or your distro's equivalent) for invisible headful Chrome on Linux",
        )
    } else {
        None
    }
}

/// Persona identity of a browser profile: the fields that decide
/// what the wire sees. Same hash = same identity = same warm slot
/// reuse; a profile change must never silently inherit another
/// persona's browser (the single-slot era reused whatever was warm,
/// which let scorecard probes run under the fetch profile's browser).
fn persona_key(profile: &BrowserProfile) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    profile.name.hash(&mut h);
    format!("{:?}", profile.tls).hash(&mut h);
    format!("{:?}", profile.h2).hash(&mut h);
    profile.user_agent.hash(&mut h);
    format!("{:?}", profile.platform).hash(&mut h);
    h.finish()
}

impl GhostManager {
    pub async fn new() -> Arc<Self> {
        Self::with_slot_default(3).await
    }

    /// Test seam: same init path, arbitrary default (clamped by the
    /// same rules as the env).
    async fn with_slot_default(default: usize) -> Arc<Self> {
        // Termux (Android) has no X11 by default. Skip Xvfb entirely;
        // Ghost will use --headless=new mode. Detecting Termux early
        // avoids a confusing error message about Xvfb installation.
        let is_termux = std::env::var_os("PREFIX")
            .map(|p| p.to_string_lossy().contains("com.termux"))
            .unwrap_or(false);

        // A forced headless backend does not need a virtual display. Avoid
        // starting Xvfb so the selection is explicit in both process and args.
        let (display, xvfb) = if super::cloak::headless_mode_requested() {
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[ghost] headless backend selected, skipping Xvfb");
            }
            (None, None)
        } else if !is_termux && super::xvfb::is_available() {
            match super::xvfb::Xvfb::start().await {
                Ok(xvfb) => {
                    let disp = xvfb.display_env();
                    if std::env::var_os("DONGHOST_DEBUG").is_some() {
                        eprintln!("[ghost] Xvfb started on {disp}");
                    }
                    (Some(disp), Some(xvfb))
                }
                Err(e) => {
                    eprintln!(
                        "[ghost] Xvfb start failed: {e}, falling back to headful off-screen mode"
                    );
                    (None, None)
                }
            }
        } else if is_termux {
            // Termux: no Xvfb needed. Ghost uses --headless=new.
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[ghost] Termux detected, using headless mode (no Xvfb)");
            }
            (None, None)
        } else if let Some(hint) = xvfb_missing_hint() {
            // Xvfb not installed on a Linux-family system: warn the
            // user. Chrome will run headful off-screen
            // (--window-position=-32000,-32000 + CDP minimize), but
            // on Linux a minimized window may still flash on screen
            // briefly. Xvfb is the clean solution for invisible
            // headful Chrome there. macOS/Windows never see this
            // hint: headful off-screen is their native mode and the
            // apt/pacman advice does not apply (issue #81).
            eprintln!("{hint}");
            (None, None)
        } else {
            // macOS/Windows/other: no Xvfb concept at all.
            (None, None)
        };

        let seed = pool_slots(default);
        let slots: Vec<Slot> = (0..seed)
            .map(|_| Slot {
                ghost: None,
                key: None,
                host: None,
            })
            .collect();
        let meta: Vec<Snap> = slots
            .iter()
            .map(|_| Snap {
                live: false,
                key: None,
                host: None,
                used: Instant::now(),
            })
            .collect();
        let mgr = Arc::new(Self {
            meta: Arc::new(Mutex::new(meta)),
            slots: slots
                .into_iter()
                .map(|s| Arc::new(AsyncMutex::new(s)))
                .collect(),
            display,
            xvfb: AsyncMutex::new(xvfb),
        });
        let reaper = Arc::clone(&mgr);
        tokio::spawn(async move { reaper.reap_loop().await });
        mgr
    }

    /// Acquire the ghost: launch if absent, thaw if frozen,
    /// relaunch if the thaw finds a corpse.
    pub async fn acquire(&self, profile: &BrowserProfile) -> Result<GhostGuard, FetchError> {
        self.acquire_for(profile, None).await
    }

    /// Host-affinity acquire: a repeat hit on the same host reuses
    /// the browser that already touched that site (session warmth),
    /// when that browser matches the persona. Different profiles or
    /// hosts spill into other slots or evict the coldest.
    pub async fn acquire_for(
        &self,
        profile: &BrowserProfile,
        host: Option<&str>,
    ) -> Result<GhostGuard, FetchError> {
        let key = persona_key(profile);
        let idx = {
            let mut snaps = self.meta.lock().unwrap_or_else(|p| p.into_inner());
            claim_slot(&mut snaps, key, host)
        };
        // Lock only the chosen slot. Other slots stay free for
        // concurrent acquires.
        let mut guard = Arc::clone(&self.slots[idx]).lock_owned().await;
        if guard.key != Some(key) {
            // Persona switch on a still-live browser: the slot's
            // browser belongs to another identity. Kill it instead
            // of mutating another persona's fingerprint state.
            if let Some(mut old) = guard.ghost.take() {
                old.kill().await;
                guard.key = None;
                guard.host = None;
            }
        }
        guard.key = Some(key);
        guard.host = host.map(|h| h.to_string());
        let need_launch = match guard.ghost.as_mut() {
            None => true,
            Some(g) => !g.thaw(),
        };
        if need_launch {
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[pool] launch slot {} (thaw fail or empty)", idx);
            }
            if let Some(mut old) = guard.ghost.take() {
                old.kill().await;
            }
            guard.ghost = Some(Ghost::launch(profile, self.display.as_deref()).await?);
        } else {
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[pool] warm serve slot {}", idx);
            }
            // Warm slot served the job: pool receipt. The kill switch
            // does not silence the counter: a single slot may warm-reuse.
            let mut st = crate::ghost::cache::GhostState::load();
            st.note_pool_served();
        }
        {
            let mut snaps = self.meta.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(snap) = snaps.get_mut(idx) {
                snap.live = guard.ghost.is_some();
                snap.key = guard.key;
                snap.host = guard.host.clone();
                snap.used = Instant::now();
            }
        }
        Ok(GhostGuard {
            meta: Arc::clone(&self.meta),
            guard,
            idx,
        })
    }

    /// Freeze every slot idle past FREEZE_AFTER; reap those past
    /// REAP_AFTER frozen. 5s tick. A busy slot (job in flight) is
    /// locked; defer its reap to the next tick.
    async fn reap_loop(&self) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            for (idx, slot) in self.slots.iter().enumerate() {
                let idle = {
                    let snaps = self.meta.lock().unwrap_or_else(|p| p.into_inner());
                    snaps.get(idx).map(|s| s.used.elapsed()).unwrap_or_default()
                };
                let Ok(mut guard) = slot.try_lock() else {
                    continue; // job in flight; defer to the next tick
                };
                let Some(g) = guard.ghost.as_mut() else {
                    if let Ok(mut snaps) = self.meta.lock()
                        && let Some(snap) = snaps.get_mut(idx)
                    {
                        snap.live = false;
                        snap.key = None;
                        snap.host = None;
                    }
                    continue;
                };
                if g.is_frozen() {
                    if idle > REAP_AFTER {
                        if std::env::var_os("DONGHOST_DEBUG").is_some() {
                            eprintln!("[pool] reap slot {}", idx);
                        }
                        if let Some(mut dead) = guard.ghost.take() {
                            dead.kill().await;
                        }
                        guard.key = None;
                        guard.host = None;
                        if let Ok(mut snaps) = self.meta.lock()
                            && let Some(snap) = snaps.get_mut(idx)
                        {
                            snap.live = false;
                            snap.key = None;
                            snap.host = None;
                        }
                    }
                } else if idle > FREEZE_AFTER {
                    g.freeze();
                    if let Ok(mut snaps) = self.meta.lock()
                        && let Some(snap) = snaps.get_mut(idx)
                    {
                        // Freeze is the start of the reap countdown: the
                        // reaper must not age a fresh freeze with the
                        // pre-freeze idle clock.
                        snap.used = Instant::now();
                    }
                }
            }
        }
    }

    /// Daemon shutdown: kill every slot's browser, then the pool Xvfb.
    pub async fn shutdown(&self) {
        for slot in &self.slots {
            let mut guard = slot.lock().await;
            if let Some(mut g) = guard.ghost.take() {
                g.kill().await;
            }
        }
        let xvfb = self.xvfb.lock().await.take();
        if let Some(xvfb) = xvfb {
            xvfb.kill().await;
        }
    }

    /// Is Xvfb active (headful mode)?
    #[allow(dead_code)]
    pub fn is_headful(&self) -> bool {
        self.display.is_some()
    }
}

/// Slot selection, pure so it stays testable without a browser.
/// Ranked: same persona + same host (session-warm reuse), then
/// same persona with no competing host affinity, then held-empty
/// same persona (no competing affinity), then a free slot (a NEW
/// host spills here : the daemon runs one profile, so ranking
/// "any warm same-persona slot" above free slots would funnel
/// every host into slot 0 forever), then coldest same-persona
/// reuse (a thaw beats a relaunch), then coldest eviction.
/// DECISION ONLY: killing a stranger persona's browser before
/// relaunching lives in acquire_for.
fn pick_slot(snaps: &[Snap], key: u64, host: Option<&str>) -> usize {
    let mine = |v: &Snap| v.key == Some(key);
    // Affinities compete only when the job and the slot both name
    // a host and the hosts differ; a hostless job or slot rides
    // along with anything.
    let compatible = |v: &Snap| match (host, v.host.as_deref()) {
        (Some(job), Some(slot)) => job == slot,
        _ => true,
    };
    if let Some(host) = host
        && let Some(i) = snaps
            .iter()
            .position(|v| v.live && mine(v) && v.host.as_deref() == Some(host))
    {
        return i;
    }
    if let Some(i) = snaps
        .iter()
        .position(|v| v.live && mine(v) && compatible(v))
    {
        return i;
    }
    // Held-empty same-persona slot (browser reaped under this
    // persona, or a same-host launch already in flight): reuse
    // before opening another slot.
    if let Some(i) = snaps
        .iter()
        .position(|v| !v.live && mine(v) && compatible(v))
    {
        return i;
    }
    // Spill: unclaimed slots first, then any browserless slot
    // (a stranger's reaped slot costs nothing to take over).
    if let Some(i) = snaps.iter().position(|v| !v.live && v.key.is_none()) {
        return i;
    }
    if let Some(i) = snaps.iter().position(|v| !v.live) {
        return i;
    }
    // Every slot is warm and affined elsewhere. Reusing our own
    // coldest browser costs a thaw; evicting a stranger's costs a
    // kill AND a launch : prefer our own.
    if let Some(i) = coldest(snaps, |v| v.live && mine(v)) {
        return i;
    }
    coldest(snaps, |_| true)
        // Only reachable with a zero-slot pool, a construction
        // bug; acquire_for would index-panic instead of spawning.
        // The pool build clamps to >=1 so this is unreachable.
        .unwrap_or(usize::MAX)
}

fn coldest(snaps: &[Snap], eligible: impl Fn(&Snap) -> bool) -> Option<usize> {
    snaps
        .iter()
        .enumerate()
        .filter(|(_, v)| eligible(v))
        .min_by(|a, b| a.1.used.cmp(&b.1.used))
        .map(|(i, _)| i)
}

/// Pick and stamp the claim (key, host, used) in one step, under
/// the caller's meta lock. The claim is visible to concurrent
/// pickers BEFORE the seconds-long browser launch, so parallel
/// jobs to different hosts spread across slots instead of all
/// stacking behind one slot's launch. The launch outcome (live)
/// lands in the snapshot after acquire finishes, as before.
fn claim_slot(snaps: &mut [Snap], key: u64, host: Option<&str>) -> usize {
    let idx = pick_slot(snaps, key, host);
    if let Some(snap) = snaps.get_mut(idx) {
        snap.key = Some(key);
        snap.host = host.map(|h| h.to_string());
        snap.used = Instant::now();
    }
    idx
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    fn v(live: bool, key: Option<u64>, host: Option<&str>, idle_s: u64) -> Snap {
        Snap {
            live,
            key,
            host: host.map(|h| h.to_string()),
            used: Instant::now() - Duration::from_secs(idle_s),
        }
    }

    // The daemon runs ONE profile, so every acquire shares one
    // persona key. If "any warm same-persona slot" outranks empty
    // slots, that one persona never opens a second slot: every job
    // to every host funnels into slot 0 and the pool never pools.
    #[test]
    fn different_hosts_spill_into_free_slots() {
        let k = 11;
        let views = vec![
            v(true, Some(k), Some("a.test"), 1),
            v(false, None, None, 0),
            v(false, None, None, 0),
        ];
        assert_eq!(
            pick_slot(&views, k, Some("b.test")),
            1,
            "a new host must open a free slot, not steal a.test's warm session"
        );
        // The reverse direction holds too: a.test keeps its slot.
        assert_eq!(pick_slot(&views, k, Some("a.test")), 0);
    }

    #[test]
    fn exhausted_pool_reuses_coldest_own_slot_before_evicting_strangers() {
        let k = 11;
        let views = vec![
            v(true, Some(k), Some("a.test"), 1),
            v(true, Some(k), Some("b.test"), 60),
            v(true, Some(22), Some("c.test"), 600),
        ];
        assert_eq!(
            pick_slot(&views, k, Some("d.test")),
            1,
            "warm same-persona reuse (no relaunch) beats killing a stranger's browser"
        );
    }

    // Selection alone is not enough: the launch takes seconds, and
    // the slot's claim used to reach the meta snapshot only after
    // it. Concurrent acquires all saw the same pre-launch snapshot
    // and stacked on one slot. claim_slot stamps the claim under
    // the caller's meta lock, before anyone launches.
    #[test]
    fn concurrent_claims_spread_hosts_instead_of_stacking() {
        let mut views = vec![
            v(false, None, None, 0),
            v(false, None, None, 0),
            v(false, None, None, 0),
        ];
        let k = 11;
        assert_eq!(claim_slot(&mut views, k, Some("a.test")), 0);
        assert_eq!(
            claim_slot(&mut views, k, Some("b.test")),
            1,
            "a second in-flight host must not stack behind a.test's launch"
        );
        assert_eq!(
            claim_slot(&mut views, k, Some("a.test")),
            0,
            "the same host joins the in-flight claim and warm-serves after it"
        );
    }

    #[test]
    fn same_persona_host_wins_over_warm_other() {
        let k = 11;
        let views = vec![
            v(true, Some(k), Some("a.test"), 30),
            v(true, Some(k), Some("b.test"), 1),
            v(true, Some(22), None, 1),
        ];
        assert_eq!(pick_slot(&views, k, Some("a.test")), 0);
    }

    #[test]
    fn same_persona_any_slot_beats_spare_launch() {
        let k = 11;
        let views = vec![v(false, None, None, 0), v(true, Some(k), None, 60)];
        assert_eq!(
            pick_slot(&views, k, None),
            1,
            "warm same-persona browser beats a launch"
        );
    }

    #[test]
    fn persona_switch_pickthen_kill_semantics() {
        // Slot belongs to persona A. Persona B picks the same slot,
        // and acquire_for must kill instead of inheriting.
        let views = vec![v(true, Some(11), Some("a.test"), 10)];
        assert_eq!(pick_slot(&views, 22, None), 0);
        assert_ne!(11, 22);
    }

    #[test]
    fn spare_slot_beats_eviction() {
        let k = 11;
        let views = vec![v(true, Some(9), None, 1), v(false, None, None, 0)];
        assert_eq!(
            pick_slot(&views, k, None),
            1,
            "empty slot beats evicting a warm browser"
        );
    }

    #[test]
    fn coldest_evicted_when_no_capacity() {
        let k = 11;
        let views = vec![v(true, Some(9), None, 3), v(true, Some(8), None, 60)];
        assert_eq!(pick_slot(&views, k, None), 1);
    }

    #[test]
    fn pool_size_kill_switch_and_clamps() {
        assert_eq!(
            pool_size(true, Some("44"), 3),
            1,
            "kill switch forces single-slot legacy"
        );
        assert_eq!(pool_size(false, Some("44"), 3), 16, "above 16 clamps to 16");
        assert_eq!(pool_size(false, Some("0"), 5), 5, "0 falls back to default");
        assert_eq!(pool_size(false, Some("7"), 3), 7);
        assert_eq!(
            pool_size(false, Some("junk"), 3),
            3,
            "unparseable falls back"
        );
        assert_eq!(pool_size(false, None, 3), 3);
        assert_eq!(pool_size(true, None, 3), 1);
    }
}

#[cfg(test)]
mod xvfb_hint_tests {
    #[test]
    fn hint_exists_only_on_linux() {
        #[cfg(target_os = "linux")]
        assert!(super::xvfb_missing_hint().is_some());
        #[cfg(not(target_os = "linux"))]
        assert!(
            super::xvfb_missing_hint().is_none(),
            "the Xvfb install hint must not exist off Linux (issue #81)"
        );
    }
}
