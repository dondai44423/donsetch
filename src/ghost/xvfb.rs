//! Xvfb virtual display manager : the stealth foundation.
//!
//! Headless Chrome (`--headless=new`) is detectable: SwiftShader
//! WebGL, missing `window.chrome`, screen dimension mismatches.
//! Headful Chrome on a virtual X display is NOT : it has real
//! GPU compositing, real window objects, real screen geometry.
//!
//! This module starts one Xvfb at daemon init and keeps it warm.
//! Ghost launches headful Chrome on this display. The display
//! outlives individual browser processes (crash → relaunch uses
//! the same display, no Xvfb restart).
//!
//! Linux-only. macOS/Windows use headful off-screen mode
//! (`--window-position=-32000,-32000`) handled in ghost/mod.rs.

// ── Linux: real Xvfb implementation ──

#[cfg(linux_like)]
mod linux {
    use std::process::Stdio;
    use tokio::process::{Child, Command};

    use crate::error::FetchError;

    /// Display number for our Xvfb. Defaults to 99: high enough to
    /// avoid collision with real displays, low enough to be a valid
    /// X display. Overridable via DONSETCH_XVFB_DISPLAY for
    /// multi-daemon hosts (two sessions sharing :99 is supported,
    /// but separate displays are cheaper when both are hot).
    fn display_num() -> u8 {
        std::env::var_os("DONSETCH_XVFB_DISPLAY")
            .and_then(|v| v.to_string_lossy().parse::<u8>().ok())
            .filter(|n| (1..=254).contains(n))
            .unwrap_or(99)
    }

    /// Startup gate so two sessions racing for a shared display
    /// cannot kill each other (issue #95): every starter that is
    /// not the winner blocks here and reuses the display once it
    /// is up. Per-display path so concurrent daemons on different
    /// displays never wait on each other.
    pub(crate) fn lock_path() -> String {
        format!("/tmp/.donsetch-xvfb-{}.lock", display_num())
    }
    /// A starter that holds the gate longer than this without
    /// producing a live display is treated as dead (stale-lock
    /// recovery, mirrored from the Windows profile lock).
    pub(crate) const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

    pub struct Xvfb {
        /// None if we reused an existing Xvfb (borrowed: don't kill).
        pub(crate) child: Option<Child>,
        /// The startup gate we created (None when reused). Held for
        /// our lifetime so a second starter knows a coordinator is
        /// alive; removed when we kill the display we own.
        pub(crate) lock_path: Option<String>,
    }

    impl Xvfb {
        /// Start Xvfb on the configured display, 1920x1080x24.
        /// Returns the DISPLAY env value for Chrome to use.
        ///
        /// If an Xvfb is already running on the display (e.g. the MCP daemon
        /// started one), reuses it, does NOT kill or restart. This
        /// is critical for CLI+MCP coexistence: the CLI must not
        /// disrupt the daemon's warm Xvfb.
        ///
        /// Startup is serialized through a create_new gate. Before
        /// this, two concurrent sessions could both run the stale-
        /// cleanup pkill and kill whichever of them got a live Xvfb
        /// up first; the loser then degraded to headful off-screen
        /// and logged a misleading "install Xvfb" diagnostic.
        pub async fn start() -> Result<Self, FetchError> {
            let display = format!(":{}", display_num());

            // Fast path: display already alive, reuse.
            if display_alive().await {
                return Ok(Self {
                    child: None,
                    lock_path: None,
                });
            }

            // Slow path: become the startup coordinator. Any other
            // starter already holding the gate means a start is in
            // flight; wait for its display instead of racing it.
            let mut gate_held = false;
            for _ in 0..120 {
                match create_gate() {
                    Ok(()) => {
                        gate_held = true;
                        break;
                    }
                    Err(Gate::Busy) => {
                        if display_alive().await {
                            return Ok(Self {
                                child: None,
                                lock_path: None,
                            });
                        }
                        let age = gate_age();
                        if age > STALE_AFTER {
                            let _ = std::fs::remove_file(lock_path());
                            continue;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(Gate::Io(error)) => {
                        return Err(FetchError::ghost(format!("Xvfb startup gate: {error}")));
                    }
                }
            }
            if !gate_held {
                // Busy for the whole budget and the display never came
                // up: honest diagnosis beats a blind second Xvfb.
                return Err(FetchError::ghost(format!(
                    "another session is starting Xvfb on the {display} display (gate held for {:?}); it has not come up",
                    gate_age()
                )));
            }

            // We hold the gate. Someone may have finished underneath
            // us while we were blocked: reuse rather than restart.
            if display_alive().await {
                let _ = std::fs::remove_file(lock_path());
                return Ok(Self {
                    child: None,
                    lock_path: None,
                });
            }

            // Kill stale Xvfb on this display (crash recovery). Safe:
            // we are the only starter allowed past the gate.
            let _ = tokio::process::Command::new("pkill")
                .args(["-f", &format!("Xvfb {display}")])
                .output()
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            // Remove stale socket + lock files. A dead Xvfb leaves
            // these behind : a new Xvfb can't bind to a stale socket,
            // and our readiness check would see the stale file and
            // think Xvfb is ready when it isn't.
            let sock_path = format!("/tmp/.X11-unix/X{}", display_num());
            let x_lock_path = format!("/tmp/.X{}-lock", display_num());
            let _ = std::fs::remove_file(&sock_path);
            let _ = std::fs::remove_file(&x_lock_path);

            // Ensure /tmp/.X11-unix/ exists. Under WSL and some minimal
            // container setups this directory is absent, and Xvfb can't
            // create the X11 socket without it.
            let _ = std::fs::create_dir_all("/tmp/.X11-unix");

            // Split the diagnostic by cause: a missing executable is
            // an install problem; a present executable that fails to
            // spawn or exits early is an environment problem. Telling
            // the user to install Xvfb in the latter case (issue #95's
            // report) hides the real failure.
            if !is_available() {
                let _ = std::fs::remove_file(lock_path());
                return Err(FetchError::ghost(
                    "Xvfb not found (install: apt install xvfb / pacman -S xorg-server-xvfb)",
                ));
            }

            let mut cmd = Command::new("Xvfb");
            cmd.args([
                &display,
                "-screen",
                "0",
                "1920x1080x24",
                "-ac",
                "-nolisten",
                "tcp",
            ]);
            cmd.stdout(Stdio::null())
                .stderr(Stdio::piped()) // kept for the failure diagnostic
                .stdin(Stdio::null());

            let mut child = cmd.spawn().map_err(|e| {
                let _ = std::fs::remove_file(lock_path());
                FetchError::ghost(format!(
                    "Xvfb spawn failed: {e} (executable found but it could not start; check display/permission setup)"
                ))
            })?;

            // Wait for the display to be ready by polling the X11
            // socket file AND verifying we can connect to it.
            // Xvfb creates /tmp/.X11-unix/X99 when it's ready to
            // accept connections. We also try connecting to make
            // sure the socket is live, not just present.
            // 10s timeout: WSL and some containers are slower to start.
            let sock_path = format!("/tmp/.X11-unix/X{}", display_num());
            let ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if std::fs::exists(&sock_path).unwrap_or(false)
                        && std::os::unix::net::UnixStream::connect(&sock_path).is_ok()
                    {
                        return;
                    }
                    // Check if Xvfb died early.
                    if child.try_wait().ok().flatten().is_some() {
                        return; // process exited : will fail below
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await;

            if ready.is_err() || !std::fs::exists(&sock_path).unwrap_or(false) {
                // Gather the real reason: Xvfb's own final words beat
                // any generic guess (bad driver, missing xkb dir,
                // permission problem : none of which are fixed by
                // "install Xvfb").
                let tail = read_stderr_tail(&mut child).await.unwrap_or_default();
                let _ = std::fs::remove_file(lock_path());
                return Err(FetchError::ghost(format!(
                    "Xvfb failed to start on {display}{}{}",
                    if tail.is_empty() { "" } else { ": " },
                    tail
                )));
            }

            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[ghost] Xvfb started on {display}");
            }
            Ok(Self {
                child: Some(child),
                lock_path: Some(lock_path()),
            })
        }

        /// The DISPLAY environment value for Chrome.
        pub fn display_env(&self) -> String {
            format!(":{}", display_num())
        }

        /// Kill Xvfb (only if we own it). Removes the startup gate so
        /// the next session is not blocked behind a dead coordinator.
        pub async fn kill(mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill().await;
            }
            if let Some(path) = self.lock_path.take() {
                let _ = std::fs::remove_file(path);
            }
        }

        /// Check if Xvfb process is still alive.
        #[allow(dead_code)]
        pub fn is_alive(&mut self) -> bool {
            match &mut self.child {
                Some(c) => c.try_wait().map(|r| r.is_none()).unwrap_or(false),
                None => true, // borrowed : assume alive
            }
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            // Safety net: if the GhostManager is dropped without
            // calling shutdown() (panic, crash, runtime exit),
            // the Xvfb child would leak. start_kill sends SIGKILL
            // synchronously, no async needed.
            if let Some(child) = &mut self.child {
                let _ = child.start_kill();
            }
            // Release the gate so a fresh session can coordinate
            // without waiting out the stale-lock recovery window.
            if let Some(path) = self.lock_path.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Gate states for the serialized Xvfb startup.
    pub(crate) enum Gate {
        /// The gate file already exists: someone else is starting.
        Busy,
        /// Unexpected filesystem problem while working with the gate.
        Io(std::io::Error),
    }

    /// Try to become the startup coordinator via create_new.
    /// Fails with Busy without touching the other starter's file.
    pub(crate) fn create_gate() -> Result<(), Gate> {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(lock_path())
        {
            Ok(_file) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(Gate::Busy),
            Err(error) => Err(Gate::Io(error)),
        }
    }

    /// Age of the gate file. A missing file reports an ancient age so
    /// the wait loop retries immediately.
    pub(crate) fn gate_age() -> std::time::Duration {
        std::fs::metadata(lock_path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(std::time::Duration::from_secs(3600))
    }

    /// Drain the remaining stderr of a failed Xvfb child into a
    /// single bounded line: the process's own final words, the only
    /// honest failure diagnostic. The child is already dead, so the
    /// read ends at EOF; the timeout only guards a weird hang.
    async fn read_stderr_tail(child: &mut Child) -> Option<String> {
        use tokio::io::AsyncReadExt;
        let mut stderr = child.stderr.take()?;
        let mut total = Vec::with_capacity(4096);
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        total.extend_from_slice(&buf[..n]);
                        if total.len() > 8192 {
                            break;
                        }
                    }
                }
            }
            total.len()
        })
        .await;
        if read.is_err() && total.is_empty() {
            return None;
        }
        tail_line(&String::from_utf8_lossy(&total))
    }

    /// Last non-blank line of `text`, trimmed, cut to ~300 bytes.
    /// The cut is pulled back onto a char boundary: Xvfb's stderr is
    /// lossy-decoded bytes, and `&line[..297]` inside a multibyte
    /// char (a localized X error, or the U+FFFD replacement the
    /// lossy decode itself inserts) was a str-slice panic on the
    /// one path that only runs when the display already failed.
    pub(super) fn tail_line(text: &str) -> Option<String> {
        let line = text
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())?;
        if line.len() > 300 {
            let cut = line.floor_char_boundary(297);
            Some(format!("{}…", &line[..cut]))
        } else {
            Some(line)
        }
    }

    /// Check if Xvfb binary is available on the system.
    pub fn is_available() -> bool {
        std::process::Command::new("which")
            .arg("Xvfb")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Check if an X display is alive by testing the X11
    /// socket file AND verifying someone is listening. A stale
    /// socket (from a killed Xvfb process) will still have the
    /// file but no server: connecting fails with ECONNREFUSED.
    async fn display_alive() -> bool {
        let sock = format!("/tmp/.X11-unix/X{}", display_num());
        if !std::fs::exists(&sock).unwrap_or(false) {
            return false;
        }
        // Socket exists: is anyone listening? Try connecting. If it
        // fails, the socket is stale.
        if std::os::unix::net::UnixStream::connect(&sock).is_err() {
            return false;
        }
        // A tombstone socket left behind by a zombie'd Xvfb (kill
        // -9 mid-session) can still answer a bare connect, and a
        // Chromium client handed that socket will never open CDP
        // (devtools ws timeout on every tier-2 escalation). A
        // bounded real-protocol probe settles it: only a server
        // that answers xdpyinfo within 2 seconds is healthy.
        if !xdpyinfo_probe().await {
            return false;
        }
        true
    }

    /// Real X-protocol probe. False when xdpyinfo is missing (the
    /// cheaper connect-only semantics still apply) or the display
    /// does not answer within the budget.
    async fn xdpyinfo_probe() -> bool {
        static PROBE_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        const PRESENT: u8 = 1;
        const MISSING: u8 = 2;
        let state = PROBE_STATE.load(std::sync::atomic::Ordering::Relaxed);
        let present = if state == PRESENT {
            true
        } else if state == MISSING {
            false
        } else {
            let ok = tokio::process::Command::new("which")
                .arg("xdpyinfo")
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);
            PROBE_STATE.store(
                if ok { PRESENT } else { MISSING },
                std::sync::atomic::Ordering::Relaxed,
            );
            ok
        };
        if !present {
            return true; // xdpyinfo absent: trust the connect check
        }
        let display = format!(":{}", display_num());
        let child = tokio::process::Command::new("xdpyinfo")
            .arg("-display")
            .arg(&display)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(_) => return true, // spawn failed: do not veto on tooling
        };
        match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) => status.success(),
            _ => {
                let _ = child.kill().await;
                false
            }
        }
    }
}

// ── Non-Linux: stub (macOS/Windows use off-screen headful mode) ──
// Android is linux_like so it uses the real Xvfb module (though
// Termux won't have Xvfb installed, the stub correctly reports
// not available and Ghost falls back to --headless=new).

#[cfg(not(linux_like))]
mod other {
    use crate::error::FetchError;

    pub struct Xvfb;

    impl Xvfb {
        pub async fn start() -> Result<Self, FetchError> {
            Err(FetchError::ghost("Xvfb not available on this platform"))
        }
        pub fn display_env(&self) -> String {
            String::new()
        }
        #[allow(dead_code)]
        pub async fn kill(self) {}
        #[allow(dead_code)]
        pub fn is_alive(&mut self) -> bool {
            false
        }
    }

    pub fn is_available() -> bool {
        false
    }
}

#[cfg(linux_like)]
pub use linux::*;
#[cfg(not(linux_like))]
pub use other::*;

#[cfg(linux_like)]
#[cfg(test)]
mod tests {
    use super::linux as x;
    use std::sync::Mutex;

    /// Tests below touch /tmp gate files and set process env vars.
    /// Serialize them within this binary (nextest runs one process).
    static SYNC_SERIAL: Mutex<()> = Mutex::new(());
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // A long final stderr line of multibyte chars must be cut on a
    // char boundary, not sliced at byte 297 (a panic on the failure
    // path, i.e. exactly when the diagnostic is needed).
    #[test]
    fn stderr_tail_cuts_long_multibyte_line_on_a_char_boundary() {
        // 3-byte chars: byte 297 is a boundary (297 = 3*99), so
        // shift by one ASCII byte to land inside a char.
        let long = format!("x{}", "終".repeat(150));
        let text = format!("first line\n\n{long}\n   \n");
        let tail = x::tail_line(&text).expect("a non-blank line");
        assert!(tail.ends_with('…'), "{tail}");
        assert!(tail.len() <= 297 + '…'.len_utf8());
        assert!(tail.starts_with("x終"));
        // Short lines pass through trimmed and untouched.
        assert_eq!(
            x::tail_line("a\n  cannot open display :99  \n").as_deref(),
            Some("cannot open display :99")
        );
        assert_eq!(x::tail_line("\n  \n"), None);
    }

    #[test]
    fn gate_conflicts_and_recovers() {
        let _lock = SYNC_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = x::lock_path();
        let _ = std::fs::remove_file(&path);
        assert!(x::create_gate().is_ok());
        assert!(matches!(x::create_gate(), Err(x::Gate::Busy)));
        assert!(x::gate_age() < x::STALE_AFTER);
        let _ = std::fs::remove_file(&path);
        assert!(x::create_gate().is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn startup_reuses_already_running_display() {
        let _lock = SERIAL.lock().await;
        // The pi daemon may own :99 on dev machines; a fresh owned
        // display proves both sides of the coin on the same box.
        // 106: real Xvfb, no fake.
        unsafe {
            std::env::set_var("DONSETCH_XVFB_DISPLAY", "106");
        }
        let owned = x::Xvfb::start().await.expect("start owned");
        assert!(owned.lock_path.is_some(), "owner holds the gate");
        let reused = x::Xvfb::start().await.expect("start reuse");
        assert!(reused.lock_path.is_none(), "reuser borrows, no gate");
        assert!(reused.child.is_none(), "reuser does not own a child");
        drop(reused);
        owned.kill().await;
        unsafe {
            std::env::remove_var("DONSETCH_XVFB_DISPLAY");
        }
    }

    #[tokio::test]
    async fn concurrent_start_serializes_and_reuses() {
        let _lock = SERIAL.lock().await;
        unsafe {
            std::env::set_var("DONSETCH_XVFB_DISPLAY", "107");
        }
        let (a, b) = tokio::join!(x::Xvfb::start(), x::Xvfb::start());
        let a = a.expect("left start");
        let b = b.expect("right start");
        // Exactly one owns; the other borrowed the winner's display
        // instead of racing the stale-cleanup pkill.
        let owners = [a.lock_path.is_some(), b.lock_path.is_some()];
        assert_eq!(
            owners.iter().filter(|o| **o).count(),
            1,
            "exactly one coordinator"
        );
        // Both believe in a display and both kill paths are safe.
        let _ = a.kill().await;
        let _ = b.kill().await;
        // Gate is gone after the owner died, display dead too.
        let gate = x::lock_path();
        assert!(!std::fs::exists(&gate).unwrap_or(false), "gate cleaned up");
        unsafe {
            std::env::remove_var("DONSETCH_XVFB_DISPLAY");
        }
    }

    #[tokio::test]
    async fn spawn_failure_reports_real_error_not_install_hint() {
        let _lock = SERIAL.lock().await;
        // A fake Xvfb that exits non-zero, exactly the issue #95
        // repro. The diagnostic must quote the child's stderr and
        // must not tell a user who HAS Xvfb to install it.
        let dir = std::env::temp_dir().join("donsetch-fake-xvfb-bin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("Xvfb");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'fake xvfb: cannot open display' >&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        unsafe {
            std::env::set_var("DONSETCH_XVFB_DISPLAY", "108");
        }
        let saved_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", format!("{}:/usr/bin:/bin", dir.display()));
        }
        let result = x::Xvfb::start().await;
        unsafe {
            std::env::remove_var("DONSETCH_XVFB_DISPLAY");
            match saved_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("fake xvfb: cannot open display"), "got: {msg}");
                assert!(
                    !msg.contains("install"),
                    "must not suggest installing Xvfb when the binary exists: {msg}"
                );
            }
            Ok(_) => panic!("fake Xvfb should have failed"),
        }
        let gate = x::lock_path();
        std::fs::remove_file(&gate).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
