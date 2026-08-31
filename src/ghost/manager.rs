//! GhostManager — the daemon's browser lifecycle brain.
//!
//! One browser, one tab, one job at a time. Frozen
//! between jobs (0 CPU), reaped after 10 min frozen,
//! crash-transparent on acquire.
//!
//! On Linux, an Xvfb virtual display is started at init
//! and kept warm. Ghost launches headful Chrome on this
//! display — the stealth path that passes Cloudflare/DataDome.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, OwnedMutexGuard};

use super::{FREEZE_AFTER, Ghost, REAP_AFTER};
use crate::error::FetchError;
use crate::profile::BrowserProfile;

struct Slot {
    ghost: Option<Ghost>,
    xvfb: Option<super::xvfb::Xvfb>,
    last_used: Instant,
}

pub struct GhostManager {
    slot: Arc<Mutex<Slot>>,
    /// Xvfb display string (":99") on Linux, None elsewhere.
    display: Option<String>,
}

/// RAII handle: derefs straight to the live Ghost, so
/// async ops hold the lock across awaits. Drop stamps
/// last_used for the reaper.
pub struct GhostGuard {
    guard: OwnedMutexGuard<Slot>,
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
        self.guard.last_used = Instant::now();
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
            let _ = self.guard.ghost.take();
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
            "[ghost] Xvfb not found — install with `apt install xvfb` or `pacman -S xorg-server-xvfb` (or your distro's equivalent) for invisible headful Chrome on Linux",
        )
    } else {
        None
    }
}

impl GhostManager {
    pub async fn new() -> Arc<Self> {
        // Termux (Android) has no X11 by default. Skip Xvfb entirely;
        // Ghost will use --headless=new mode. Detecting Termux early
        // avoids a confusing error message about Xvfb installation.
        let is_termux = std::env::var_os("PREFIX")
            .map(|p| p.to_string_lossy().contains("com.termux"))
            .unwrap_or(false);

        // A forced headless backend does not need a virtual display. Avoid
        // starting Xvfb so the selection is explicit in both process and args.
        let xvfb = if super::cloak::headless_mode_requested() {
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[ghost] headless backend selected, skipping Xvfb");
            }
            None
        } else if !is_termux && super::xvfb::is_available() {
            match super::xvfb::Xvfb::start().await {
                Ok(xvfb) => {
                    let disp = xvfb.display_env();
                    if std::env::var_os("DONGHOST_DEBUG").is_some() {
                        eprintln!("[ghost] Xvfb started on {disp}");
                    }
                    Some(xvfb)
                }
                Err(e) => {
                    eprintln!(
                        "[ghost] Xvfb start failed: {e}, falling back to headful off-screen mode"
                    );
                    None
                }
            }
        } else if is_termux {
            // Termux: no Xvfb needed. Ghost uses --headless=new.
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[ghost] Termux detected, using headless mode (no Xvfb)");
            }
            None
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
            None
        } else {
            // macOS/Windows/other: no Xvfb concept at all.
            None
        };

        let display = xvfb.as_ref().map(|x| x.display_env());

        let mgr = Arc::new(Self {
            slot: Arc::new(Mutex::new(Slot {
                ghost: None,
                xvfb,
                last_used: Instant::now(),
            })),
            display,
        });
        let reaper = Arc::clone(&mgr);
        tokio::spawn(async move { reaper.reap_loop().await });
        mgr
    }

    /// Acquire the ghost: launch if absent, thaw if
    /// frozen, relaunch if the thaw finds a corpse.
    pub async fn acquire(&self, profile: &BrowserProfile) -> Result<GhostGuard, FetchError> {
        let mut slot = self.slot.clone().lock_owned().await;
        let need_launch = match slot.ghost.as_mut() {
            None => true,
            Some(g) => !g.thaw(),
        };
        if need_launch {
            if let Some(mut old) = slot.ghost.take() {
                old.kill().await;
            }
            slot.ghost = Some(Ghost::launch(profile, self.display.as_deref()).await?);
        }
        Ok(GhostGuard { guard: slot })
    }

    /// Freeze after FREEZE_AFTER idle; reap after
    /// REAP_AFTER frozen. 5s tick.
    async fn reap_loop(&self) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let mut slot = self.slot.lock().await;
            let idle = slot.last_used.elapsed();
            let Some(g) = slot.ghost.as_mut() else {
                continue;
            };
            if g.is_frozen() {
                if idle > REAP_AFTER {
                    let mut g = slot.ghost.take().expect("ghost");
                    g.kill().await;
                }
            } else if idle > FREEZE_AFTER {
                g.freeze();
            }
        }
    }

    /// Daemon shutdown: kill browser + Xvfb (if owned).
    pub async fn shutdown(&self) {
        let mut slot = self.slot.lock().await;
        if let Some(mut g) = slot.ghost.take() {
            g.kill().await;
        }
        if let Some(xvfb) = slot.xvfb.take() {
            xvfb.kill().await;
        }
    }

    /// Is Xvfb active (headful mode)?
    #[allow(dead_code)]
    pub fn is_headful(&self) -> bool {
        self.display.is_some()
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
