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

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use super::{FREEZE_AFTER, Ghost, REAP_AFTER};
use crate::error::FetchError;
use crate::profile::BrowserProfile;

/// Pool size: default 3 warm slots. Env override
/// `browser.pool_slots` clamps to 1..=16; 0 falls back to the
/// default (a zero-slot pool would disable the warm path entirely,
/// which the kill switch owns). The legacy `DONSETCH_NO_GHOST_POOL`
/// kill switch maps to the same single-slot path through the config
/// layer.
fn pool_slots(default: usize) -> usize {
    let n = crate::config::cfg().browser.pool_slots;
    pool_size(n == 1, (n > 1).then(|| n.to_string()).as_deref(), default)
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
    reserved: bool,
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
    /// Wire identity (viewport + locale) this browser was launched
    /// with. A warm ghost whose wire differs from the incoming
    /// persona (quarantine → re-mint changed locale/viewport) must
    /// relaunch: tier-1 already sent the new Accept-Language.
    wire: Option<crate::ghost::GhostWire>,
}

pub struct GhostManager {
    meta: Arc<Mutex<Vec<Snap>>>,
    available: Arc<Semaphore>,
    slots: Vec<Arc<AsyncMutex<Slot>>>,
    /// Xvfb display string (":99") on Linux, None elsewhere. The
    /// display is pool-wide: every slot's Chrome attaches to it.
    /// Lazily initialized on the FIRST browser acquire: a tier-1-only
    /// process must not pay Xvfb probes (`which Xvfb`, an `xdpyinfo`
    /// spawn, an X11 connect, or an Xvfb start) for a browser it will
    /// never launch. Outer None = not decided yet; Some(None) = decided
    /// headless; Some(Some(env)) = a display that is re-verified on
    /// every later acquire, because a display that died mid-session
    /// must be replaced, never replayed as a tombstone.
    display: AsyncMutex<Option<Option<String>>>,
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
    // Fields drop in declaration order: release the slot lock before making
    // its reservation available to a waiter.
    _reservation: Reservation,
    idx: usize,
    pub queue_wait: Duration,
    pub reused: bool,
}

pub(crate) struct BrowserRead {
    pub guard: GhostGuard,
    pub page: super::ops::GhostPage,
    pub recovery: Option<ReadRecovery>,
}

pub(crate) struct ReadRecovery {
    pub reason: &'static str,
    pub elapsed: Duration,
}

struct Reservation {
    meta: Arc<Mutex<Vec<Snap>>>,
    idx: usize,
    touch: bool,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut snaps = self.meta.lock().unwrap_or_else(|p| p.into_inner());
        snaps[self.idx].reserved = false;
        if self.touch {
            snaps[self.idx].used = Instant::now();
        }
        // The owned semaphore permit releases after this metadata update.
    }
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
        if self
            .guard
            .ghost
            .as_ref()
            .is_some_and(|g| g.is_dirty() || g.link_dead())
        {
            // Drop owns and kills only this browser tree. A cancelled job's
            // late document/events can never contaminate the next reservation.
            self.guard.ghost = None;
            self.guard.key = None;
            self.guard.host = None;
            self.guard.wire = None;
        }
        if self.guard.ghost.is_none() {
            self.guard.key = None;
            self.guard.host = None;
            self.guard.wire = None;
        }
        // Stamp the slot we held. tokio's blocking_lock is safe in a
        // drop path (contended only across job lifetimes, the meta
        // critical section is microseconds).
        if let Ok(mut snaps) = self.meta.lock()
            && let Some(snap) = snaps.get_mut(self.idx)
        {
            snap.used = Instant::now();
            snap.live = self.guard.ghost.is_some();
            snap.key = self.guard.key;
            snap.host = self.guard.host.clone();
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

fn read_retry_reason(error: &FetchError, dead: bool, dirty: bool) -> Option<&'static str> {
    // A broken browser does not turn a policy/name/proxy failure into permission
    // to navigate again. Only automation transport errors earn a read retry.
    let FetchError::Ghost(message) = error else {
        return None;
    };
    if message.contains("ERR_")
        || !(message.starts_with("cdp ")
            || message.starts_with("browser pass deadline exceeded")
            || message.starts_with("browser navigation deadline exceeded"))
    {
        return None;
    }
    if dead {
        Some("transport-dead")
    } else if dirty {
        Some("cancelled-generation")
    } else if message.starts_with("cdp timeout:") {
        Some("cdp-timeout")
    } else {
        None
    }
}

/// Persona identity of a browser profile: the fields that decide
/// what the wire sees. Same hash = same identity = same warm slot
/// reuse; a profile change must never silently inherit another
/// persona's browser (the single-slot era reused whatever was warm,
/// which let scorecard probes run under the fetch profile's browser).
fn persona_key(profile: &BrowserProfile, wire: &crate::ghost::GhostWire) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    profile.name.hash(&mut h);
    format!("{:?}", profile.tls).hash(&mut h);
    format!("{:?}", profile.h2).hash(&mut h);
    profile.user_agent.hash(&mut h);
    format!("{:?}", profile.platform).hash(&mut h);
    wire.hash(&mut h);
    h.finish()
}

impl GhostManager {
    pub async fn new() -> Arc<Self> {
        Self::with_slot_default(3).await
    }

    /// Test seam: same init path, arbitrary default (clamped by the
    /// same rules as the env).
    async fn with_slot_default(default: usize) -> Arc<Self> {
        let seed = pool_slots(default);
        let slots: Vec<Slot> = (0..seed)
            .map(|_| Slot {
                ghost: None,
                key: None,
                host: None,
                wire: None,
            })
            .collect();
        let meta: Vec<Snap> = slots
            .iter()
            .map(|_| Snap {
                reserved: false,
                live: false,
                key: None,
                host: None,
                used: Instant::now(),
            })
            .collect();
        let mgr = Arc::new(Self {
            meta: Arc::new(Mutex::new(meta)),
            available: Arc::new(Semaphore::new(seed)),
            slots: slots
                .into_iter()
                .map(|s| Arc::new(AsyncMutex::new(s)))
                .collect(),
            display: AsyncMutex::new(None),
            xvfb: AsyncMutex::new(None),
        });
        let reaper = Arc::clone(&mgr);
        tokio::spawn(async move { reaper.reap_loop().await });
        mgr
    }

    /// Start (or adopt) the pool display on first need, then keep it
    /// honest: every later call re-verifies the display before handing
    /// it to Chrome, because a display that died mid-session (crash,
    /// oom, a borrowed display whose owner exited) must be replaced,
    /// never replayed as a tombstone for the rest of the process.
    async fn ensure_display(&self) -> Option<String> {
        // Termux (Android) has no X11 by default. Skip Xvfb entirely;
        // Ghost will use --headless=new mode.
        let is_termux = std::env::var_os("PREFIX")
            .map(|p| p.to_string_lossy().contains("com.termux"))
            .unwrap_or(false);
        // A forced headless backend does not need a virtual display.
        // Avoid starting Xvfb so the selection is explicit in both
        // process and args.
        if super::cloak::headless_mode_requested() {
            if crate::config::cfg().debug.ghost {
                eprintln!("[ghost] headless backend selected, skipping Xvfb");
            }
            return None;
        }
        if is_termux {
            if crate::config::cfg().debug.ghost {
                eprintln!("[ghost] Termux detected, using headless mode (no Xvfb)");
            }
            return None;
        }
        let decided = self.display.lock().await.clone();
        match decided {
            // Decided headless (no Xvfb install): stable for the
            // process, exactly like the old OnceCell's cached None.
            Some(None) => return None,
            Some(Some(disp)) => {
                if super::xvfb::display_alive().await {
                    return Some(disp);
                }
                // The display died mid-session: retire it, then fall
                // through to a fresh start below.
                eprintln!("[ghost] Xvfb {disp} is gone; restarting the display");
                self.retire_display().await;
            }
            None => {}
        }
        if !super::xvfb::is_available() {
            // Xvfb not installed on a Linux-family system: say so once.
            // Ghost then runs Chromium headless (no display needed) for
            // the life of this process; macOS/Windows never see this
            // hint (issue #81).
            if let Some(hint) = xvfb_missing_hint() {
                eprintln!("{hint}");
            }
            *self.display.lock().await = Some(None);
            return None;
        }
        match super::xvfb::Xvfb::start().await {
            Ok(xvfb) => {
                let disp = xvfb.display_env();
                if crate::config::cfg().debug.ghost {
                    // A borrowed display was reused, not started: a
                    // pre-existing X server is not ours, and saying
                    // "started" for it was wrong (#258).
                    if xvfb.is_borrowed() {
                        eprintln!("[ghost] Xvfb reused on {disp} (already running)");
                    } else {
                        eprintln!("[ghost] Xvfb started on {disp}");
                    }
                }
                *self.xvfb.lock().await = Some(xvfb);
                *self.display.lock().await = Some(Some(disp.clone()));
                Some(disp)
            }
            Err(e) => {
                if cfg!(target_os = "linux") {
                    eprintln!("[ghost] Xvfb start failed: {e}, falling back to headless mode");
                } else {
                    eprintln!(
                        "[ghost] Xvfb start failed: {e}, falling back to headful off-screen mode"
                    );
                }
                *self.display.lock().await = Some(None);
                None
            }
        }
    }

    /// Drop the cached display decision (and kill whatever owned the
    /// display), so the next ensure_display derives a fresh one. Used
    /// when a launch proves the display unusable even after the
    /// one-shot headless retry.
    async fn retire_display(&self) {
        let mut chosen = self.display.lock().await;
        if chosen.as_ref().is_some_and(Option::is_some) {
            if let Some(old) = self.xvfb.lock().await.take() {
                old.kill().await;
            }
            *chosen = None;
        }
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
        self.acquire_for_wire(profile, host, crate::ghost::GhostWire::default())
            .await
    }

    /// Host-affinity acquire with a persona-coherent wire identity
    /// (viewport + locale). Used by web_fetch / screenshot so the
    /// ghost agrees with tier-1 Accept-Language and the persona pin.
    pub async fn acquire_for_wire(
        &self,
        profile: &BrowserProfile,
        host: Option<&str>,
        mut wire: crate::ghost::GhostWire,
    ) -> Result<GhostGuard, FetchError> {
        if wire.direct {
            wire.route = Some(crate::transport::request_route::RequestRoute::direct());
        } else if wire.route.is_none() {
            let proxy = if crate::config::cfg().proxy.fetch_rotate {
                crate::search::egress::global()
                    .and_then(|pool| pool.pick_fetch(host.unwrap_or_default(), true))
                    .and_then(|lane| lane.proxy)
            } else {
                None
            };
            wire.route = Some(
                proxy
                    .map(crate::transport::request_route::RequestRoute::pinned)
                    .unwrap_or_else(crate::transport::request_route::RequestRoute::configured),
            );
        }
        // Configuration fails before slot reservation or display/browser startup.
        wire.route
            .as_ref()
            .expect("route selected")
            .browser_proxies()?;
        let key = persona_key(profile, &wire);
        let queued = Instant::now();
        let reservation = self.reserve(key, host).await?;
        let queue_wait = queued.elapsed();
        let idx = reservation.idx;
        // Lock only the chosen slot. Other slots stay free for
        // concurrent acquires.
        let guard = Arc::clone(&self.slots[idx]).lock_owned().await;
        // Own cleanup before any launch/kill/display await. Acquisition may
        // be cancelled or fail before there is a browser to return.
        let mut held = GhostGuard {
            meta: Arc::clone(&self.meta),
            guard,
            _reservation: reservation,
            idx,
            queue_wait,
            reused: false,
        };
        let guard = &mut held.guard;
        if guard.key != Some(key) {
            // Persona switch on a still-live browser: the slot's
            // browser belongs to another identity. Kill it instead
            // of mutating another persona's fingerprint state.
            if let Some(mut old) = guard.ghost.take() {
                old.kill().await;
                guard.key = None;
                guard.host = None;
                guard.wire = None;
            }
        }
        // Warm ghost whose viewport/locale no longer matches the
        // incoming persona (quarantine re-mint) must relaunch: the
        // browser's navigator.languages / window size would disagree
        // with the Accept-Language tier-1 already sent.
        if guard.wire.as_ref().is_some_and(|w| *w != wire) {
            if crate::config::cfg().debug.ghost {
                eprintln!("[pool] wire mismatch on warm slot, relaunching");
            }
            if let Some(mut old) = guard.ghost.take() {
                old.kill().await;
            }
            guard.key = None;
            guard.host = None;
            guard.wire = None;
        }
        guard.key = Some(key);
        guard.host = host.map(|h| h.to_string());
        guard.wire = Some(wire.clone());
        // A live process with a dead DevTools link is not a warm
        // browser: every call on it would only time out.
        let need_launch = match guard.ghost.as_mut() {
            None => true,
            Some(g) => !g.thaw() || g.link_dead(),
        };
        if need_launch {
            if crate::config::cfg().debug.ghost {
                eprintln!("[pool] launch slot {} (thaw fail or empty)", idx);
            }
            if let Some(mut old) = guard.ghost.take() {
                old.kill().await;
            }
            let display = self.ensure_display().await;
            match Ghost::launch_wire(profile, display.as_deref(), &wire).await {
                Ok(ghost) => guard.ghost = Some(ghost),
                Err(error) => {
                    // The display-class failure that survived the one-shot
                    // headless retry: retire the display so the next call
                    // derives a fresh one instead of replaying the corpse.
                    if display.is_some()
                        && crate::ghost::is_platform_init_failure(&error.to_string())
                    {
                        self.retire_display().await;
                    }
                    return Err(error);
                }
            }
        } else {
            if crate::config::cfg().debug.ghost {
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
        held.reused = !need_launch;
        Ok(held)
    }

    /// Read a document before actions, with at most one replacement under the
    /// original read deadline. A wall result is decisive and is never retried here.
    pub(crate) async fn read_document(
        &self,
        mut guard: GhostGuard,
        profile: &BrowserProfile,
        url: &str,
        timeout: Duration,
    ) -> Result<BrowserRead, FetchError> {
        let started = Instant::now();
        let deadline = started
            .checked_add(timeout)
            .ok_or_else(|| FetchError::ghost("invalid browser read timeout"))?;
        if timeout.is_zero() {
            return Err(FetchError::ghost("browser read deadline exhausted"));
        }
        let first = super::ops::ghost_fetch(&mut guard, url, timeout).await;
        let mut recovery = None;
        let page = match first {
            Ok(page) => page,
            Err(error) => {
                let Some(reason) = read_retry_reason(&error, guard.link_dead(), guard.is_dirty())
                else {
                    return Err(error);
                };
                if Instant::now() >= deadline {
                    return Err(error);
                }
                let wire = guard.guard.wire.clone().expect("acquired browser wire");
                let host = guard.guard.host.clone();
                if crate::config::cfg().debug.ghost {
                    eprintln!("[browser-read] replacing browser after {reason}: {error}");
                }
                // A timed-out live link is retired too: re-acquiring its slot
                // must not return the same generation with late replies in flight.
                guard.guard.ghost = None;
                drop(guard);
                guard = tokio::time::timeout_at(
                    deadline.into(),
                    self.acquire_for_wire(profile, host.as_deref(), wire),
                )
                .await
                .map_err(|_| FetchError::ghost("browser replacement deadline exhausted"))??;
                recovery = Some(ReadRecovery {
                    reason,
                    elapsed: started.elapsed(),
                });
                super::ops::ghost_fetch(
                    &mut guard,
                    url,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .await?
            }
        };
        Ok(BrowserRead {
            guard,
            page,
            recovery,
        })
    }

    async fn reserve(&self, key: u64, host: Option<&str>) -> Result<Reservation, FetchError> {
        // FIFO admission waits on global availability, never on a chosen busy
        // host. Cancelling a waiter removes it from the semaphore's queue.
        let permit = Arc::clone(&self.available)
            .acquire_owned()
            .await
            .map_err(|_| FetchError::ghost("browser pool closed"))?;
        let idx = {
            let mut snaps = self.meta.lock().unwrap_or_else(|p| p.into_inner());
            claim_slot(&mut snaps, key, host)
                .ok_or_else(|| FetchError::ghost("browser pool reservation invariant failed"))?
        };
        Ok(Reservation {
            meta: Arc::clone(&self.meta),
            idx,
            touch: true,
            _permit: permit,
        })
    }

    fn try_reserve_idle(&self, idx: usize) -> Option<(Reservation, Duration)> {
        let permit = Arc::clone(&self.available).try_acquire_owned().ok()?;
        let mut snaps = self.meta.lock().unwrap_or_else(|p| p.into_inner());
        let snap = snaps.get_mut(idx)?;
        let idle = snap.used.elapsed();
        if snap.reserved || !snap.live || idle <= FREEZE_AFTER {
            return None;
        }
        snap.reserved = true;
        Some((
            Reservation {
                meta: Arc::clone(&self.meta),
                idx,
                touch: false,
                _permit: permit,
            },
            idle,
        ))
    }

    /// Freeze every slot idle past FREEZE_AFTER; reap those past
    /// REAP_AFTER frozen. 5s tick. A busy slot (job in flight) is
    /// locked; defer its reap to the next tick.
    async fn reap_loop(&self) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if self.available.is_closed() {
                break;
            }
            for (idx, slot) in self.slots.iter().enumerate() {
                // Maintenance owns capacity too. A job cannot claim this
                // slot between the idle check and an awaited graceful reap.
                let Some((_maintenance, idle)) = self.try_reserve_idle(idx) else {
                    continue;
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
                        if crate::config::cfg().debug.ghost {
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
        self.available.close();
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

    /// Is Xvfb active (headful mode)? False before the first acquire
    /// has initialized the display.
    #[allow(dead_code)]
    pub fn is_headful(&self) -> bool {
        self.display
            .try_lock()
            .ok()
            .is_some_and(|d| d.as_ref().is_some_and(Option::is_some))
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
fn pick_slot(snaps: &[Snap], key: u64, host: Option<&str>) -> Option<usize> {
    let mine = |v: &Snap| !v.reserved && v.key == Some(key);
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
        return Some(i);
    }
    if let Some(i) = snaps
        .iter()
        .position(|v| v.live && mine(v) && compatible(v))
    {
        return Some(i);
    }
    // Held-empty same-persona slot (browser reaped under this
    // persona, or a same-host launch already in flight): reuse
    // before opening another slot.
    if let Some(i) = snaps
        .iter()
        .position(|v| !v.live && mine(v) && compatible(v))
    {
        return Some(i);
    }
    // Spill: unclaimed slots first, then any browserless slot
    // (a stranger's reaped slot costs nothing to take over).
    if let Some(i) = snaps
        .iter()
        .position(|v| !v.reserved && !v.live && v.key.is_none())
    {
        return Some(i);
    }
    if let Some(i) = snaps.iter().position(|v| !v.reserved && !v.live) {
        return Some(i);
    }
    // Every slot is warm and affined elsewhere. Reusing our own
    // coldest browser costs a thaw; evicting a stranger's costs a
    // kill AND a launch : prefer our own.
    if let Some(i) = coldest(snaps, |v| v.live && mine(v)) {
        return Some(i);
    }
    coldest(snaps, |v| !v.reserved)
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
fn claim_slot(snaps: &mut [Snap], key: u64, host: Option<&str>) -> Option<usize> {
    let idx = pick_slot(snaps, key, host)?;
    let snap = &mut snaps[idx];
    snap.reserved = true;
    snap.key = Some(key);
    snap.host = host.map(str::to_owned);
    snap.used = Instant::now();
    Some(idx)
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires installed Chromium; retires only its own two private browsers"]
    async fn stealth_v3_native_teardown_preserves_another_slots_zygotes() {
        let mut config = crate::config::DonsetchConfig::default();
        config.browser.backend = crate::config::BrowserBackend::Headless;
        config.browser.cloak_auto_download = false;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
        let profile = BrowserProfile::chrome(151, crate::profile::Platform::Linux, false);
        let first = Ghost::launch(&profile, None).await.unwrap();
        let first_pid = first.pid().unwrap();
        let file = crate::paths::cache_dir().join("owned-teardown.html");
        std::fs::write(&file, "<h1>Owned surviving browser document</h1>").unwrap();
        let url = url::Url::from_file_path(&file).unwrap().to_string();
        first.navigate_raw(&url, false).await.unwrap();
        assert!(
            first
                .outer_html()
                .await
                .unwrap()
                .contains("Owned surviving")
        );
        let mut second = Ghost::launch(&profile, None).await.unwrap();
        assert_ne!(second.pid(), first.pid());
        assert!(first.temp_profile.is_none());
        assert!(second.temp_profile.is_some());
        // The native handler advertises its PID in this browser's own
        // zygote argv. Never infer ownership from a shared crash database.
        let handler_pid = |browser: u32| -> i32 {
            for task in std::fs::read_dir(format!("/proc/{browser}/task"))
                .unwrap()
                .flatten()
            {
                let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                    continue;
                };
                for child in children.split_whitespace() {
                    let Ok(cmdline) = std::fs::read(format!("/proc/{child}/cmdline")) else {
                        continue;
                    };
                    let command = String::from_utf8_lossy(&cmdline).replace('\0', " ");
                    if let Some(pid) = command.split_whitespace().find_map(|arg| {
                        arg.strip_prefix("--crashpad-handler-pid=")?
                            .parse::<i32>()
                            .ok()
                    }) {
                        assert!(pid > 0);
                        let executable = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
                        assert_eq!(executable.file_name().unwrap(), "chrome_crashpad_handler");
                        return pid;
                    }
                }
            }
            panic!("native browser must expose its own crash handler PID");
        };
        let first_handler = handler_pid(first_pid);
        let second_handler = handler_pid(second.pid().unwrap());
        assert_ne!(first_handler, second_handler);
        second.kill().await;
        drop(second);
        // Give native process death/events a chance to arrive; a response
        // queued before teardown would not prove the renderer survived.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let live = |pid: i32| {
            std::fs::read_to_string(format!("/proc/{pid}/status"))
                .is_ok_and(|status| !status.contains("State:\tZ"))
        };
        assert!(live(first_handler));
        assert!(
            !live(second_handler),
            "native handler exits when its owned browser closes"
        );
        let next = crate::paths::cache_dir().join("owned-teardown-next.html");
        std::fs::write(&next, "<h1>Owned new renderer after peer retirement</h1>").unwrap();
        first
            .navigate_raw(url::Url::from_file_path(next).unwrap().as_str(), false)
            .await
            .unwrap();
        assert!(
            first
                .outer_html()
                .await
                .unwrap()
                .contains("Owned new renderer")
        );
        first.navigate_raw(&url, false).await.unwrap();
        assert!(
            first
                .outer_html()
                .await
                .unwrap()
                .contains("Owned surviving")
        );
        assert_eq!(first.pid(), Some(first_pid));
        assert!(!first.link_dead());
        let third = Ghost::launch(&profile, None).await.unwrap();
        let third_handler = handler_pid(third.pid().unwrap());
        assert_ne!(first_handler, third_handler);
        drop(third);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(live(first_handler));
        assert!(
            !live(third_handler),
            "hard teardown also releases the native handler"
        );
        first.navigate_raw(&url, false).await.unwrap();
        assert!(
            first
                .outer_html()
                .await
                .unwrap()
                .contains("Owned surviving")
        );
        assert!(!first.link_dead());
        drop(first);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a native EGL-capable GPU; owns a fresh private browser"]
    async fn stealth_v3_native_headless_webgl_renders_actual_pixels() {
        native_webgl_pixels(crate::config::BrowserBackend::Headless).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires Xvfb and a native EGL-capable GPU; owns a private browser"]
    async fn stealth_v3_native_xvfb_webgl_renders_actual_pixels() {
        native_webgl_pixels(crate::config::BrowserBackend::Chromium).await;
    }

    #[cfg(target_os = "linux")]
    async fn native_webgl_pixels(backend: crate::config::BrowserBackend) {
        let mut config = crate::config::DonsetchConfig::default();
        config.browser.backend = backend;
        config.browser.cloak_auto_download = false;
        config.browser.pool_slots = 1;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
        let profile = BrowserProfile::chrome(151, crate::profile::Platform::Linux, false);
        let mgr = GhostManager::new().await;
        let g = mgr.acquire_for(&profile, Some("owned.test")).await.unwrap();
        let results = g.eval_json("['webgl','webgl2'].map(kind=>{const c=document.createElement('canvas');c.width=c.height=16;const gl=c.getContext(kind);if(!gl)return {kind,available:false};gl.clearColor(18/255,52/255,86/255,1);gl.clear(gl.COLOR_BUFFER_BIT);const pixel=new Uint8Array(4);gl.readPixels(0,0,1,1,gl.RGBA,gl.UNSIGNED_BYTE,pixel);const e=gl.getExtension('WEBGL_debug_renderer_info');return {kind,available:true,pixel:[...pixel],error:gl.getError(),renderer:e?gl.getParameter(e.UNMASKED_RENDERER_WEBGL):null}})").await.unwrap();
        assert_eq!(results.as_array().unwrap().len(), 2);
        for result in results.as_array().unwrap() {
            assert_eq!(
                result["available"], true,
                "native context unavailable: {result}"
            );
            assert_eq!(result["pixel"], serde_json::json!([18, 52, 86, 255]));
            assert_eq!(result["error"], 0);
            assert!(!result["renderer"].as_str().unwrap().is_empty());
        }
        drop(g);
        mgr.shutdown().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires an installed Chromium; owns a local HTTP origin and private browser"]
    async fn stealth_v3_native_documents_reload_actions_and_terminal_tiny_walls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let test_started = Instant::now();
        let mut config = crate::config::DonsetchConfig::default();
        config.browser.backend = crate::config::BrowserBackend::Headless;
        config.browser.cloak_auto_download = false;
        config.browser.pool_slots = 1;
        config.proxy.from_environment = false;
        config.debug.ghost = true;
        // This process owns the fixture origin. Product defaults remain strict.
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = seen.clone();
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let requests = requests.clone();
                connections.spawn(async move {
                    let mut bytes = Vec::new();
                    while bytes.len() < 16 * 1024 {
                        let mut buf = [0; 2048];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 { return; }
                        bytes.extend_from_slice(&buf[..n]);
                        if bytes.windows(4).any(|w| w == b"\r\n\r\n") { break; }
                    }
                    let request = String::from_utf8_lossy(&bytes);
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    requests.lock().unwrap().push(path.clone());
                    let (status, body) = match path.as_str() {
                        "/a" => (200, format!("<article><h1>Old document A</h1><p>{}</p></article>", "Old fixture content. ".repeat(30))),
                        "/b" => {
                            tokio::time::sleep(Duration::from_millis(800)).await;
                            (201, format!("<article><h1>New document B</h1><p>{}</p></article>", "New fixture content. ".repeat(30)))
                        }
                        "/spa" => (202, format!("<button id='next' onclick=\"document.querySelector('article').innerHTML='<h1>Interacted document</h1><p>{}</p>';document.querySelector('article').style.backgroundColor='#123456'\">Next</button><article><h1>Before interaction</h1><p>{}</p></article>", "Visible new content. ".repeat(80), "Old article content. ".repeat(80))),
                        "/human" => (403, "<h1>Verify you are human</h1><form><div class='g-recaptcha'></div></form>".into()),
                        "/bootstrap" => (200, "<script>window.KPSDK={};</script><script src='/sensor/ips.js?x-kpsdk-im=opaque'></script>".into()),
                        "/empty" => (200, "<html><body><main id='root'></main></body></html>".into()),
                        "/redirect" => (200, "<script>location.replace('/redirect-final')</script><h1>Transient document</h1>".into()),
                        "/redirect-final" => (201, format!("<article><h1>Redirected native document</h1><p>{}</p></article>", "Redirected useful content. ".repeat(80))),
                        "/auth" => (401, format!("<article><h1>Restricted content</h1><p>{}</p></article>", "Ordinary looking response. ".repeat(80))),
                        "/paywall" => (402, "<h1>Payment required</h1>".into()),
                        "/status404" => (404, format!("<article><h1>Ordinary article</h1><p>{}</p></article>", "A missing page must not become content. ".repeat(80))),
                        _ => (404, "<h1>Not found</h1>".into()),
                    };
                    let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
                while connections.try_join_next().is_some() {}
            }
        });
        let mgr = GhostManager::new().await;
        let profile = BrowserProfile::chrome(151, crate::profile::Platform::Linux, false);
        eprintln!("[owned-doc] acquiring {:?}", test_started.elapsed());
        let mut g = mgr.acquire_for(&profile, Some("127.0.0.1")).await.unwrap();
        eprintln!("[owned-doc] A navigation {:?}", test_started.elapsed());
        g.navigate(&format!("{base}/a")).await.unwrap();
        let a = g.document();
        assert_eq!(a.status, Some(200));
        let started = Instant::now();
        eprintln!("[owned-doc] B navigation {:?}", test_started.elapsed());
        g.navigate(&format!("{base}/b")).await.unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(800),
            "A completed B before B responded"
        );
        let b = g.document();
        assert!(b.generation > a.generation);
        assert_ne!(b.loader, a.loader);
        assert_eq!(b.url, format!("{base}/b"));
        assert_eq!(b.status, Some(201));
        let html = g.outer_html().await.unwrap();
        assert!(html.contains("New document B") && !html.contains("Old document A"));
        eprintln!("[owned-doc] B navigation {:?}", test_started.elapsed());
        g.navigate(&format!("{base}/b")).await.unwrap();
        let reloaded = g.document();
        assert!(reloaded.generation > b.generation);
        assert_ne!(reloaded.loader, b.loader);
        assert_eq!(reloaded.status, Some(201));
        eprintln!("[owned-doc] SPA render {:?}", test_started.elapsed());
        let page =
            super::super::ops::ghost_fetch(&mut g, &format!("{base}/spa"), Duration::from_secs(8))
                .await
                .unwrap();
        assert_eq!(page.outcome, super::super::ops::BrowserOutcome::Content);
        assert_eq!(page.document.status, Some(202));
        let geometry = g
            .eval_json("({x:screenX,y:screenY,outerWidth,outerHeight,hidden:document.hidden})")
            .await
            .unwrap();
        assert_eq!(geometry["x"], 0);
        assert_eq!(geometry["y"], 0);
        assert!(geometry["outerWidth"].as_u64().unwrap() > 0);
        assert!(geometry["outerHeight"].as_u64().unwrap() > 0);
        assert_eq!(geometry["hidden"], false);
        let actions = [
            super::super::actions::Action::Click {
                selector: Some("#next".into()),
                text: None,
            },
            super::super::actions::Action::WaitText {
                text: "Interacted document".into(),
                timeout_ms: 2000,
            },
        ];
        eprintln!("[owned-doc] actions {:?}", test_started.elapsed());
        super::super::actions::run(&mut g, &actions).await.unwrap();
        eprintln!("[owned-doc] actions done {:?}", test_started.elapsed());
        let html = g.outer_html().await.unwrap();
        assert!(
            html.contains("<h1>Interacted document</h1>")
                && !html.contains("<h1>Before interaction</h1>")
        );
        let point = g.eval_json("(()=>{const r=document.querySelector('article').getBoundingClientRect();return {x:r.x+1,y:r.y+1}})()").await.unwrap();
        let png = g.screenshot_bytes(false).await.unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgb8();
        let pixel = image.get_pixel(
            point["x"].as_f64().unwrap() as u32,
            point["y"].as_f64().unwrap() as u32,
        );
        assert_eq!(
            pixel.0,
            [0x12, 0x34, 0x56],
            "capture must paint the actual interacted document"
        );
        assert_eq!(g.document().generation, page.document.generation);
        assert!(
            !g.window_minimized
                .load(std::sync::atomic::Ordering::Acquire),
            "capture must not minimize a native hidden surface"
        );
        let started = Instant::now();
        eprintln!("[owned-doc] human {:?}", test_started.elapsed());
        let human = super::super::ops::ghost_fetch(
            &mut g,
            &format!("{base}/human"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(
            human.outcome,
            super::super::ops::BrowserOutcome::HumanRequired
        );
        assert!(human.html.len() < 500);
        assert_eq!(human.document.status, Some(403));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "tiny human wall waited for render deadline"
        );
        let started = Instant::now();
        assert!(matches!(
            super::super::ops::solve(&mut g, &format!("{base}/human"), Duration::from_secs(5))
                .await
                .unwrap(),
            super::super::ops::SolveOutcome::CaptchaWalled
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "legacy solve must also stop on a tiny human wall"
        );
        for (path, status, outcome) in [
            ("auth", 401, super::super::ops::BrowserOutcome::AuthRequired),
            ("paywall", 402, super::super::ops::BrowserOutcome::Paywall),
            (
                "status404",
                404,
                super::super::ops::BrowserOutcome::NotFound,
            ),
        ] {
            let page = super::super::ops::ghost_fetch(
                &mut g,
                &format!("{base}/{path}"),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
            assert_eq!(page.document.status, Some(status));
            assert_eq!(
                page.outcome, outcome,
                "native status {status} must outrank ordinary-looking DOM"
            );
        }
        let redirected = super::super::ops::ghost_fetch(
            &mut g,
            &format!("{base}/redirect"),
            Duration::from_secs(8),
        )
        .await
        .unwrap();
        assert_eq!(redirected.document.url, format!("{base}/redirect-final"));
        assert_eq!(redirected.document.status, Some(201));
        assert_eq!(
            redirected.outcome,
            super::super::ops::BrowserOutcome::Content
        );
        assert!(
            redirected.html.contains("Redirected native document")
                && !redirected.html.contains("Transient document")
        );
        eprintln!("[owned-doc] bootstrap {:?}", test_started.elapsed());
        let bootstrap = super::super::ops::ghost_fetch(
            &mut g,
            &format!("{base}/bootstrap"),
            Duration::from_millis(1500),
        )
        .await
        .unwrap();
        assert_eq!(
            bootstrap.outcome,
            super::super::ops::BrowserOutcome::ManagedChallenge
        );
        assert_eq!(bootstrap.vendor.as_deref(), Some("kasada"));
        eprintln!("[owned-doc] empty {:?}", test_started.elapsed());
        let empty = super::super::ops::ghost_fetch(
            &mut g,
            &format!("{base}/empty"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(empty.outcome, super::super::ops::BrowserOutcome::Incomplete);
        assert_eq!(empty.document.status, Some(200));
        let file = crate::paths::cache_dir().join("owned-status-unavailable.html");
        std::fs::write(&file, "<h1>Owned file document</h1>").unwrap();
        g.navigate_raw(url::Url::from_file_path(file).unwrap().as_str(), false)
            .await
            .unwrap();
        assert_eq!(
            g.document().status,
            None,
            "file response has no HTTP status"
        );
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|p| p.as_str() == "/b")
                .count(),
            2
        );
        eprintln!("[owned-doc] teardown {:?}", test_started.elapsed());
        drop(g);
        mgr.shutdown().await;
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires an installed Chromium; owns a fresh private browser"]
    async fn stealth_v3_native_cancel_retires_owned_browser_before_reuse() {
        let mut config = crate::config::DonsetchConfig::default();
        config.browser.backend = crate::config::BrowserBackend::Headless;
        config.browser.cloak_auto_download = false;
        config.browser.pool_slots = 1;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
        let profile = BrowserProfile::chrome(151, crate::profile::Platform::Linux, false);
        let mgr = GhostManager::new().await;
        let mut g = mgr.acquire_for(&profile, Some("owned.test")).await.unwrap();
        let first_pid = g.pid().unwrap();
        let file = crate::paths::cache_dir().join("owned-cancel.html");
        std::fs::write(
            &file,
            "<h1>Owned cancellation fixture</h1><p>Fresh document.</p>",
        )
        .unwrap();
        let url = url::Url::from_file_path(&file).unwrap().to_string();
        g.navigate_raw(&url, false).await.unwrap();
        let visible = [crate::ghost::actions::Action::WaitText {
            text: "Owned cancellation fixture".into(),
            timeout_ms: 2000,
        }];
        assert!(crate::ghost::actions::run(&mut g, &visible).await.is_ok());
        let absent = [crate::ghost::actions::Action::WaitText {
            text: "This text does not exist".into(),
            timeout_ms: 100,
        }];
        let failure = crate::ghost::actions::run(&mut g, &absent)
            .await
            .unwrap_err();
        assert_eq!(failure.0, 0);
        assert!(failure.1.contains("timeout"));
        assert!(
            !g.is_dirty(),
            "a completed negative action is different from cancellation"
        );
        let actions = [crate::ghost::actions::Action::Wait { ms: 30_000 }];
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                crate::ghost::actions::run(&mut g, &actions)
            )
            .await
            .is_err()
        );
        assert!(
            g.is_dirty(),
            "the cancelled action reached its wait and contaminated this generation"
        );
        drop(g);
        assert!(
            mgr.slots[0].lock().await.ghost.is_none(),
            "cancelled process must be retired before releasing capacity"
        );
        let mut fresh = mgr.acquire_for(&profile, Some("owned.test")).await.unwrap();
        assert_ne!(fresh.pid().unwrap(), first_pid);
        assert!(!fresh.reused);
        fresh.navigate_raw(&url, false).await.unwrap();
        assert!(
            crate::ghost::actions::run(&mut fresh, &visible)
                .await
                .is_ok()
        );
        assert!(!fresh.is_dirty());
        let second_pid = fresh.pid().unwrap();
        // This PID belongs to the fresh process acquired above, never an
        // ambient/user browser. Exercise an actual abrupt transport loss.
        assert_eq!(
            unsafe { libc::kill(-(second_pid as i32), libc::SIGKILL) },
            0
        );
        for _ in 0..50 {
            if fresh.link_dead() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(fresh.link_dead());
        let failed_at = Instant::now();
        let error = crate::ghost::ops::ghost_fetch(
            &mut fresh,
            "https://owned.test/",
            Duration::from_secs(5),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("cdp link closed"));
        assert!(error.to_string().contains("transport="));
        assert!(error.to_string().contains("browser="));
        assert!(error.to_string().contains("generation="));
        assert!(failed_at.elapsed() < Duration::from_secs(1));
        drop(fresh);
        let mut replacement = mgr.acquire_for(&profile, Some("owned.test")).await.unwrap();
        assert_ne!(replacement.pid().unwrap(), second_pid);
        replacement.navigate_raw(&url, false).await.unwrap();
        assert!(
            crate::ghost::actions::run(&mut replacement, &visible)
                .await
                .is_ok()
        );
        drop(replacement);
        mgr.shutdown().await;
        assert!(mgr.slots[0].lock().await.ghost.is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires installed Chromium; kills only its own browser and reads an owned HTTP peer"]
    async fn stealth_v3_native_read_replaces_a_dead_generation_with_same_wire() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.browser.backend = crate::config::BrowserBackend::Headless;
        config.browser.cloak_auto_download = false;
        config.browser.pool_slots = 1;
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/owned", listener.local_addr().unwrap());
        let active_pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let kill_pid = active_pid.clone();
        let server = tokio::spawn(async move {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept(), if handlers.len() < 8 => {
                        let (mut stream, _) = accepted.unwrap();
                        let seen = seen.clone();
                        let kill_pid = kill_pid.clone();
                        handlers.spawn(async move {
                            let mut request = Vec::new();
                            while !request.ends_with(b"\r\n\r\n") {
                                assert!(request.len() < 8192);
                                match tokio::time::timeout(Duration::from_secs(3), stream.read_u8()).await {
                                    Ok(Ok(byte)) => request.push(byte),
                                    Ok(Err(error)) if request.is_empty() && matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => return None,
                                    Err(_) if request.is_empty() => return None, // Idle preconnect.
                                    other => panic!("incomplete owned request: {other:?}"),
                                }
                            }
                            let request = String::from_utf8(request).unwrap();
                            let path = request.split_whitespace().nth(1).unwrap().to_owned();
                            let ordinal = {
                                let mut seen = seen.lock().unwrap();
                                seen.push(path.clone());
                                assert!(seen.len() <= 12, "owned peer request bound");
                                seen.iter().filter(|p| **p == path).count()
                            };
                            if path == "/during" && ordinal == 1 {
                                let pid = kill_pid.load(std::sync::atomic::Ordering::Acquire);
                                assert!(pid > 0, "only the acquired fixture browser may be killed");
                                assert_eq!(unsafe { libc::kill(-pid, libc::SIGKILL) }, 0);
                                return Some(path);
                            }
                            let (status, body) = match path.as_str() {
                                "/owned" | "/during" => (201, format!("<html><body><article><h1>Owned recovered generation</h1><p>{}</p></article></body></html>",
                                    "This is useful owned research content with a real status from the new browser. ".repeat(30))),
                                "/human" => (403, "<h1>Verify you are human</h1><form><div class='g-recaptcha'></div></form>".into()),
                                "/human-article" => (403, format!("<title>It needs a human touch</title><h1>It needs a human touch</h1><article><div id='px-captcha'><iframe></iframe></div><p>{}</p></article>", "Complete the verification shown below before browsing. Here is additional help for displaying the verification widget. ".repeat(40))),
                                "/favicon.ico" => (404, String::new()),
                                _ => panic!("unexpected owned request {path}"),
                            };
                            let head = format!(
                                "HTTP/1.1 {status} Owned\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            stream.write_all(head.as_bytes()).await.unwrap();
                            stream.write_all(body.as_bytes()).await.unwrap();
                            Some(path)
                        });
                    }
                    finished = handlers.join_next(), if !handlers.is_empty() => {
                        if finished.unwrap().unwrap().as_deref() == Some("/human-article") {
                            return seen.lock().unwrap().clone();
                        }
                    }
                }
            }
        });
        let profile = BrowserProfile::chrome(151, crate::profile::Platform::Linux, false);
        let manager = GhostManager::new().await;
        let wire = crate::ghost::GhostWire {
            viewport: (1280, 800),
            locale: "fr-FR".into(),
            direct: true,
            route: Some(crate::transport::request_route::RequestRoute::direct()),
            ..Default::default()
        };
        let guard = manager
            .acquire_for_wire(&profile, Some("127.0.0.1"), wire.clone())
            .await
            .unwrap();
        let old = guard.pid().unwrap();
        assert_eq!(unsafe { libc::kill(-(old as i32), libc::SIGKILL) }, 0);
        for _ in 0..100 {
            if guard.link_dead() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            guard.link_dead(),
            "the actual killed process must reach EOF"
        );
        let read = manager
            .read_document(guard, &profile, &url, Duration::from_secs(15))
            .await
            .unwrap();
        assert_ne!(read.guard.pid().unwrap(), old);
        assert_eq!(
            read.guard.wire, wire,
            "replacement must retain viewport, locale and routing"
        );
        assert_eq!(read.page.document.status, Some(201));
        assert_eq!(read.page.document.url, url);
        assert_eq!(
            read.page.outcome,
            super::super::ops::BrowserOutcome::Content
        );
        assert!(read.page.html.contains("Owned recovered generation"));
        assert_eq!(read.recovery.unwrap().reason, "transport-dead");
        active_pid.store(
            read.guard.pid().unwrap() as i32,
            std::sync::atomic::Ordering::Release,
        );
        let read = manager
            .read_document(
                read.guard,
                &profile,
                &url.replace("/owned", "/during"),
                Duration::from_secs(15),
            )
            .await
            .unwrap();
        assert_ne!(
            read.guard.pid().unwrap() as i32,
            active_pid.load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(read.guard.wire, wire);
        assert_eq!(read.page.document.status, Some(201));
        assert_eq!(read.page.document.url, url.replace("/owned", "/during"));
        assert!(read.page.html.contains("Owned recovered generation"));
        assert_eq!(read.recovery.unwrap().reason, "transport-dead");
        let current_pid = read.guard.pid().unwrap();
        let started = Instant::now();
        let human = manager
            .read_document(
                read.guard,
                &profile,
                &url.replace("/owned", "/human"),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "human wall must not consume the retry deadline"
        );
        assert_eq!(human.guard.pid().unwrap(), current_pid);
        assert!(human.recovery.is_none());
        assert_eq!(
            human.page.outcome,
            super::super::ops::BrowserOutcome::HumanRequired
        );
        assert_eq!(human.page.document.status, Some(403));
        let started = Instant::now();
        let human = manager
            .read_document(
                human.guard,
                &profile,
                &url.replace("/owned", "/human-article"),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "human help text must not satisfy content settling"
        );
        assert_eq!(human.guard.pid().unwrap(), current_pid);
        assert!(human.recovery.is_none());
        assert_eq!(
            human.page.outcome,
            super::super::ops::BrowserOutcome::HumanRequired
        );
        assert_eq!(human.page.document.status, Some(403));
        let seen = server.await.unwrap();
        assert_eq!(seen.iter().filter(|p| p.as_str() == "/owned").count(), 1);
        assert_eq!(seen.iter().filter(|p| p.as_str() == "/during").count(), 2);
        assert_eq!(seen.iter().filter(|p| p.as_str() == "/human").count(), 1);
        assert_eq!(
            seen.iter()
                .filter(|p| p.as_str() == "/human-article")
                .count(),
            1
        );
        let error = manager
            .read_document(
                human.guard,
                &profile,
                "file:///etc/passwd",
                Duration::from_secs(5),
            )
            .await
            .err()
            .unwrap();
        assert!(
            matches!(error, FetchError::InvalidUrl(_) | FetchError::Ssrf(_)),
            "{error}"
        );
        let healthy = manager
            .acquire_for_wire(&profile, Some("127.0.0.1"), wire)
            .await
            .unwrap();
        assert_eq!(
            healthy.pid().unwrap(),
            current_pid,
            "policy failure must not relaunch"
        );
        assert!(healthy.reused);
        let started = Instant::now();
        let error = manager
            .read_document(healthy, &profile, &url, Duration::ZERO)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("deadline exhausted"));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(
            manager.slots[0]
                .lock()
                .await
                .ghost
                .as_ref()
                .unwrap()
                .pid()
                .unwrap(),
            current_pid,
            "an exhausted budget must not navigate or replace a healthy browser"
        );
        manager.shutdown().await;
    }

    #[test]
    fn stealth_v3_read_recovery_does_not_retry_policy_network_or_access_failures() {
        for error in [
            FetchError::Ssrf("owned policy block".into()),
            FetchError::Dns("NXDOMAIN".into()),
            FetchError::DnsTimeout("resolver".into()),
            FetchError::InvalidUrl("file:///owned".into()),
            FetchError::Timeout,
            FetchError::ghost("navigation: net::ERR_PROXY_CONNECTION_FAILED"),
            FetchError::ghost("navigation: net::ERR_SOCKS_CONNECTION_FAILED"),
            FetchError::ghost("navigation: net::ERR_TUNNEL_CONNECTION_FAILED"),
        ] {
            assert_eq!(read_retry_reason(&error, true, true), None, "{error}");
        }
        for message in [
            "navigation: net::ERR_NAME_NOT_RESOLVED",
            "interactive captcha requires a human",
            "browser document incomplete",
        ] {
            assert_eq!(
                read_retry_reason(&FetchError::ghost(message), true, true),
                None
            );
        }
        assert_eq!(
            read_retry_reason(
                &FetchError::ghost("browser pass deadline exceeded before usable content"),
                false,
                false
            ),
            None
        );
        assert_eq!(
            read_retry_reason(
                &FetchError::ghost("cdp timeout: DOM.getDocument"),
                false,
                false
            ),
            Some("cdp-timeout")
        );
        assert_eq!(
            read_retry_reason(&FetchError::ghost("cdp link closed"), true, false),
            Some("transport-dead")
        );
        assert_eq!(
            read_retry_reason(
                &FetchError::ghost("browser pass deadline exceeded before usable content"),
                false,
                true
            ),
            Some("cancelled-generation")
        );
    }

    fn v(live: bool, key: Option<u64>, host: Option<&str>, idle_s: u64) -> Snap {
        Snap {
            reserved: false,
            live,
            key,
            host: host.map(|h| h.to_string()),
            used: Instant::now() - Duration::from_secs(idle_s),
        }
    }

    #[test]
    fn stealth_v3_pool_identity_includes_native_wire_context() {
        let profile = BrowserProfile::chrome(151, crate::profile::Platform::Linux, false);
        let wire = crate::ghost::GhostWire::default();
        let original = persona_key(&profile, &wire);
        assert_eq!(original, persona_key(&profile, &wire.clone()));
        let mut different = wire.clone();
        different.direct = true;
        assert_ne!(original, persona_key(&profile, &different));
        different = wire.clone();
        different.locale = "de-DE".into();
        assert_ne!(original, persona_key(&profile, &different));
        different = wire.clone();
        different.route = Some(crate::transport::request_route::RequestRoute::pinned(
            crate::transport::proxy::Proxy::parse("http://alice:owned-secret@proxy.example:8080")
                .unwrap(),
        ));
        let routed = persona_key(&profile, &different);
        assert_ne!(original, routed);
        different.route = Some(crate::transport::request_route::RequestRoute::pinned(
            crate::transport::proxy::Proxy::parse("http://alice:changed@proxy.example:8080")
                .unwrap(),
        ));
        assert_ne!(routed, persona_key(&profile, &different));
        different = wire;
        different.viewport = (1280, 720);
        assert_ne!(original, persona_key(&profile, &different));
    }

    #[tokio::test]
    async fn stealth_v3_failed_acquisition_clears_metadata_before_capacity_returns() {
        let meta = Arc::new(Mutex::new(vec![v(false, Some(11), Some("old.test"), 0)]));
        let available = Arc::new(Semaphore::new(1));
        let permit = available.clone().acquire_owned().await.unwrap();
        meta.lock().unwrap()[0].reserved = true;
        let slot = Arc::new(AsyncMutex::new(Slot {
            ghost: None,
            key: Some(11),
            host: Some("old.test".into()),
            wire: Some(crate::ghost::GhostWire::default()),
        }));
        let held = GhostGuard {
            meta: meta.clone(),
            guard: slot.clone().lock_owned().await,
            _reservation: Reservation {
                meta: meta.clone(),
                idx: 0,
                touch: true,
                _permit: permit,
            },
            idx: 0,
            queue_wait: Duration::ZERO,
            reused: false,
        };
        assert_eq!(available.available_permits(), 0);
        drop(held);
        assert_eq!(available.available_permits(), 1);
        let snap = &meta.lock().unwrap()[0];
        assert!(!snap.reserved && !snap.live);
        assert_eq!(snap.key, None);
        assert_eq!(snap.host, None);
        assert!(slot.try_lock().unwrap().wire.is_none());
    }

    #[test]
    fn stealth_v3_busy_affinity_never_beats_an_idle_slot() {
        let mut views = vec![
            v(true, Some(11), Some("a.test"), 0),
            v(false, None, None, 0),
        ];
        views[0].reserved = true;
        assert_eq!(
            pick_slot(&views, 11, Some("a.test")),
            Some(1),
            "host affinity cannot queue behind a busy browser"
        );
    }

    #[tokio::test]
    async fn stealth_v3_waiter_takes_released_slot_and_cancel_releases_capacity() {
        let mgr = Arc::new(GhostManager {
            meta: Arc::new(Mutex::new(vec![v(false, None, None, 0); 3])),
            available: Arc::new(Semaphore::new(3)),
            slots: Vec::new(), // Reservation test: no process or slot lock needed.
            display: AsyncMutex::new(None),
            xvfb: AsyncMutex::new(None),
        });
        let slow = mgr.reserve(11, Some("slow.test")).await.unwrap();
        let short = mgr.reserve(11, Some("short.test")).await.unwrap();
        let other = mgr.reserve(11, Some("other.test")).await.unwrap();
        let released = short.idx;
        let mut waiter = Box::pin(mgr.reserve(11, Some("slow.test")));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err(),
            "all three jobs hold reservations"
        );
        drop(short);
        let job = tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            job.idx, released,
            "waiter must use the released short slot while slow stays busy"
        );
        let mut cancelled = Box::pin(mgr.reserve(11, None));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut cancelled)
                .await
                .is_err()
        );
        drop(cancelled);
        drop(job);
        let next = tokio::time::timeout(Duration::from_millis(100), mgr.reserve(11, None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.idx, released);
        drop(next);
        drop(slow);
        drop(other);
        assert_eq!(mgr.available.available_permits(), 3);
        assert!(mgr.meta.lock().unwrap().iter().all(|s| !s.reserved));
        {
            let mut snaps = mgr.meta.lock().unwrap();
            snaps[0].live = true;
            snaps[0].used = Instant::now() - Duration::from_secs(40);
        }
        let (maintenance, _) = mgr.try_reserve_idle(0).unwrap();
        assert!(
            mgr.try_reserve_idle(0).is_none(),
            "maintenance cannot reserve an owned slot twice"
        );
        let job = mgr.reserve(11, None).await.unwrap();
        assert_ne!(
            job.idx, maintenance.idx,
            "a job must not queue behind an awaited reap"
        );
        drop(job);
        drop(maintenance);
        assert!(
            mgr.meta.lock().unwrap()[0].used.elapsed() >= Duration::from_secs(40),
            "maintenance must not keep postponing its own reap clock"
        );
        let held = [
            mgr.reserve(11, None).await.unwrap(),
            mgr.reserve(11, None).await.unwrap(),
            mgr.reserve(11, None).await.unwrap(),
        ];
        let mut blocked = Box::pin(mgr.reserve(11, None));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );
        mgr.shutdown().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), blocked)
                .await
                .unwrap()
                .is_err(),
            "shutdown must wake queued jobs with a closed-pool error"
        );
        drop(held);
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
            Some(1),
            "a new host must open a free slot, not steal a.test's warm session"
        );
        // The reverse direction holds too: a.test keeps its slot.
        assert_eq!(pick_slot(&views, k, Some("a.test")), Some(0));
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
            Some(1),
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
        assert_eq!(claim_slot(&mut views, k, Some("a.test")), Some(0));
        assert_eq!(
            claim_slot(&mut views, k, Some("b.test")),
            Some(1),
            "a second in-flight host must not stack behind a.test's launch"
        );
        assert_eq!(
            claim_slot(&mut views, k, Some("a.test")),
            Some(2),
            "a busy same-host claim must spill into the remaining idle slot"
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
        assert_eq!(pick_slot(&views, k, Some("a.test")), Some(0));
    }

    #[test]
    fn same_persona_any_slot_beats_spare_launch() {
        let k = 11;
        let views = vec![v(false, None, None, 0), v(true, Some(k), None, 60)];
        assert_eq!(
            pick_slot(&views, k, None),
            Some(1),
            "warm same-persona browser beats a launch"
        );
    }

    #[test]
    fn persona_switch_pickthen_kill_semantics() {
        // Slot belongs to persona A. Persona B picks the same slot,
        // and acquire_for must kill instead of inheriting.
        let views = vec![v(true, Some(11), Some("a.test"), 10)];
        assert_eq!(pick_slot(&views, 22, None), Some(0));
        assert_ne!(11, 22);
    }

    #[test]
    fn spare_slot_beats_eviction() {
        let k = 11;
        let views = vec![v(true, Some(9), None, 1), v(false, None, None, 0)];
        assert_eq!(
            pick_slot(&views, k, None),
            Some(1),
            "empty slot beats evicting a warm browser"
        );
    }

    #[test]
    fn coldest_evicted_when_no_capacity() {
        let k = 11;
        let views = vec![v(true, Some(9), None, 3), v(true, Some(8), None, 60)];
        assert_eq!(pick_slot(&views, k, None), Some(1));
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

/// Real-Xvfb restart coverage: a display that dies mid-session must be
/// replaced by ensure_display, not replayed as a tombstone (the state
/// that left every later launch dying in ~80ms with "Missing X server
/// or $DISPLAY"). Linux-only: Xvfb is the only real virtual display.
#[cfg(linux_like)]
#[cfg(test)]
mod display_recovery_tests {
    use super::*;

    /// Live Xvfb serving `display`, by /proc cmdline.
    #[cfg(target_os = "linux")]
    fn find_xvfb_pid(display: &str) -> Option<u32> {
        for entry in std::fs::read_dir("/proc").ok()? {
            let entry = entry.ok()?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            let parts: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
            if parts.len() >= 2 && parts[0].ends_with(b"Xvfb") && parts[1] == display.as_bytes() {
                return Some(pid);
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dead_display_is_replaced_not_replayed() {
        // 105: outside the displays the ghost::xvfb tests own (99, 106-109).
        unsafe {
            std::env::set_var("DONSETCH_XVFB_DISPLAY", "105");
        }
        let mgr = GhostManager::with_slot_default(1).await;
        let first = mgr.ensure_display().await.expect("display comes up");
        assert_eq!(first, ":105");
        let owned = find_xvfb_pid(":105").expect("owned Xvfb is live");

        // Kill the display outright: the crash/oom/cleanup class.
        unsafe {
            libc::kill(owned as libc::pid_t, libc::SIGKILL);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while super::super::xvfb::display_alive().await {
            assert!(
                std::time::Instant::now() < deadline,
                "the killed display still answers: tombstone probe is broken"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let second = mgr
            .ensure_display()
            .await
            .expect("display must be replaced");
        assert_eq!(second, ":105");
        assert!(
            super::super::xvfb::display_alive().await,
            "the replacement must answer the X protocol"
        );
        let replacement = find_xvfb_pid(":105").expect("replacement Xvfb is live");
        assert_ne!(replacement, owned, "a fresh process must serve the display");

        mgr.shutdown().await;
        unsafe {
            std::env::remove_var("DONSETCH_XVFB_DISPLAY");
        }
    }
}
