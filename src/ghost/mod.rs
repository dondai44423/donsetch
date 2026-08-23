//! DonGhost — the tier-2 ghost browser.
//!
//! A real Chromium, driven over raw CDP with zero
//! automation flags and zero script injection. Exists for
//! exactly two jobs DonShadow can't do:
//!   SOLVE  — pass a JS challenge, harvest clearance
//!            cookies, hand them to tier 1.
//!   RENDER — execute a JS-rendered page, hand the DOM
//!            HTML to DonSift.
//!
//! Lifecycle: lazy launch → freeze (SIGSTOP the process
//! group, 0 CPU, swappable RAM) between jobs → reap after
//! 10 min frozen. The persistent profile dir keeps cookie
//! warmth across restarts.

pub mod actions;
pub mod cache;
pub mod cdp;
pub mod manager;
pub mod ops;
pub mod proc;
pub mod xvfb;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use serde_json::{Value, json};
#[cfg(linux_like)]
use std::os::unix::process::CommandExt as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::FetchError;
use crate::profile::BrowserProfile;

/// Idle this long → SIGSTOP the process group.
/// (Daemon lifecycle — used by the MCP idle reaper.)
pub const FREEZE_AFTER: std::time::Duration = std::time::Duration::from_secs(20);
/// Frozen this long → reap entirely.
pub const REAP_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

pub struct Ghost {
    child: Child,
    proc: proc::Proc,
    pub cdp: cdp::Cdp,
    /// Attached page session id.
    pub session: String,
    /// Our page target id.
    target: String,
    frozen: bool,
    pub last_used: Instant,
}

/// Persistent profile dir: aged state passes challenges
/// easier, and clearance cookies survive daemon restarts.
pub fn profile_dir() -> PathBuf {
    crate::paths::cache_dir().join("ghost-profile")
}

/// Locate a Chrome-family binary. Env override first, then
/// platform-specific known paths, then PATH search. No `which`
/// subprocess — works on Linux, macOS, and Windows.
pub fn chrome_binary() -> Result<String, FetchError> {
    if let Some(p) = std::env::var_os("DONGHOST_CHROME") {
        return Ok(p.to_string_lossy().into_owned());
    }
    // Known install locations (most reliable, no PATH needed).
    for path in known_chrome_paths() {
        if is_executable(&path) {
            // Snap wrappers (/snap/bin/chromium → /usr/bin/snap) are
            // executable but don't reliably pass CDP flags through
            // Snap's confinement. Resolve to the real binary.
            if let Some(real) = resolve_snap_chrome(&path) {
                return Ok(real.to_string_lossy().into_owned());
            }
            return Ok(path.to_string_lossy().into_owned());
        }
    }
    // PATH search.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in chrome_names() {
                let candidate = dir.join(name);
                if is_executable(&candidate) {
                    return Ok(candidate.to_string_lossy().into_owned());
                }
            }
        }
    }
    Err(FetchError::ghost(
        "no chromium/chrome binary found (set DONGHOST_CHROME)",
    ))
}

#[cfg(linux_like)]
fn known_chrome_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/google-chrome-stable"),
        // Snap Chromium: the real binary inside the snap mount.
        // The /snap/bin/chromium wrapper is a symlink to /usr/bin/snap
        // and doesn't reliably pass CDP flags through Snap's confinement.
        // Prefer the real binary; fall back to the wrapper below.
        PathBuf::from("/snap/chromium/current/usr/lib/chromium-browser/chrome"),
        PathBuf::from("/snap/bin/chromium"),
    ];
    // Playwright cache: ~/.cache/ms-playwright/chromium-*/chrome-linux/chrome
    // Many devs have Chromium via `npx playwright install` but not as a
    // system package. Auto-discover it so `donsetch doctor` just works.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let base = home.join(".cache/ms-playwright");
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("chromium-") {
                    let candidate = entry.path().join("chrome-linux/chrome");
                    if candidate.is_file() {
                        paths.push(candidate);
                    }
                }
            }
        }
    }
    // Termux: $PREFIX/bin/chromium-browser or chromium.
    // $PREFIX is /data/data/com.termux/files/usr.
    if let Some(prefix) = std::env::var_os("PREFIX") {
        let prefix = PathBuf::from(prefix);
        let p1 = prefix.join("bin/chromium-browser");
        let p2 = prefix.join("bin/chromium");
        // Insert at front so Termux paths are tried first.
        paths.insert(0, p1);
        paths.insert(1, p2);
    }
    paths
}

/// If `path` is a Snap wrapper (canonicalizes to /usr/bin/snap or
/// similar), resolve to the real Chromium binary inside the snap
/// mount. Returns None if `path` is already a real binary or if the
/// real binary can't be found.
#[cfg(linux_like)]
fn resolve_snap_chrome(path: &std::path::Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    let real_str = real.to_string_lossy();
    // Snap wrappers resolve to the snap command itself.
    if !real_str.ends_with("/snap") {
        return None; // Already a real binary.
    }
    // Extract the snap package name from the original path.
    // /snap/bin/chromium → "chromium", /snap/bin/firefox → "firefox"
    let orig = path.to_string_lossy();
    let snap_name = orig.strip_prefix("/snap/bin/").unwrap_or("chromium");
    // Look for the real binary inside the snap mount.
    for candidate in [
        format!("/snap/{snap_name}/current/usr/lib/chromium-browser/chrome"),
        format!("/snap/{snap_name}/current/usr/lib/{snap_name}/chromium"),
    ] {
        let p = PathBuf::from(&candidate);
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(not(linux_like))]
fn resolve_snap_chrome(_path: &std::path::Path) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn known_chrome_paths() -> Vec<PathBuf> {
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(windows)]
fn known_chrome_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // Chrome (system + per-user installs).
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            paths.push(PathBuf::from(&pf).join("Google\\Chrome\\Application\\chrome.exe"));
        }
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        let la = PathBuf::from(&la);
        paths.push(la.join("Google\\Chrome\\Application\\chrome.exe"));
        paths.push(la.join("Chromium\\Application\\chrome.exe"));
        // Playwright cache (same layout as the Linux discovery):
        // %LOCALAPPDATA%\\ms-playwright\\chromium-*\\chrome-win\\chrome.exe
        let pw = la.join("ms-playwright");
        if let Ok(entries) = std::fs::read_dir(&pw) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("chromium-") {
                    let candidate = entry.path().join("chrome-win\\chrome.exe");
                    if candidate.is_file() {
                        paths.push(candidate);
                    }
                }
            }
        }
    }
    // Edge: Chromium-based and pre-installed on Windows — often the
    // ONLY CDP-capable browser on a stock box. Its directory is never
    // on PATH, so it must be probed explicitly.
    if let Some(pfx86) = std::env::var_os("ProgramFiles(x86)") {
        paths.push(PathBuf::from(&pfx86).join("Microsoft\\Edge\\Application\\msedge.exe"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        paths.push(PathBuf::from(&pf).join("Microsoft\\Edge\\Application\\msedge.exe"));
    }
    paths
}

#[cfg(windows)]
fn chrome_names() -> &'static [&'static str] {
    // Edge is Chromium-based, pre-installed on Windows — often
    // the only available CDP-capable browser.
    &["chrome.exe", "msedge.exe", "chromium.exe"]
}

#[cfg(not(windows))]
fn chrome_names() -> &'static [&'static str] {
    &[
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
    ]
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

impl Ghost {
    /// Launch cold. Headful Chrome on Xvfb (Linux) — the real
    /// stealth mode. Headful has real WebGL, real window.chrome,
    /// real screen geometry. Headless is detectable; headful on
    /// a virtual display is not. Falls back to `--headless=new`
    /// on macOS/Windows where Xvfb is unavailable.
    ///
    /// Clean by construction: no automation flags, UA pinned to
    /// the DonShadow profile so harvested cookies stay valid
    /// when tier 1 reuses them (cf_clearance binds IP+UA).
    pub async fn launch(
        profile: &BrowserProfile,
        display: Option<&str>,
    ) -> Result<Self, FetchError> {
        // Unused on macOS/Windows (Xvfb is Linux-only) — clippy -Dwarnings errors on it.
        #[cfg(not(linux_like))]
        let _ = display;

        let bin = chrome_binary()?;
        let dir = profile_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| FetchError::ghost(format!("profile dir: {e}")))?;
        // Stale singleton files from a SIGKILLed ghost
        // (e.g. outer timeout) block the next launch.
        for f in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let mut cmd = Command::new(bin);
        let mut chrome_args: Vec<String> = vec![
            "--remote-debugging-port=0".into(),
            format!("--user-data-dir={}", dir.display()),
            format!("--user-agent={}", profile.user_agent),
            "--window-size=1920,1080".into(),
            "--window-position=-32000,-32000".into(),
            "--lang=en-US".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-background-networking".into(),
            "--disable-component-update".into(),
            "--disable-sync".into(),
            "--no-sandbox".into(),
            "--disable-setuid-sandbox".into(),
            "--disable-translate".into(),
            "--mute-audio".into(),
            // ── Disk cache suppression ──
            // Chrome's disk cache grows unboundedly (1GB+ in days).
            // We don't need it — tier 1 has its own HTTP cache, and
            // ghost only renders a few pages per domain. Disabling
            // keeps the profile dir tiny (~10MB instead of 1GB+).
            "--disk-cache-size=1".into(),
            "--disable-gpu-shader-disk-cache".into(),
            "--disable-features=SiteEngagementService".into(),
        ];
        // ── HTTP proxy (env var) ──
        // If HTTP_PROXY/HTTPS_PROXY/ALL_PROXY is set, route the
        // Ghost browser through the same proxy as tier 1. Chrome
        // handles proxy auth via its own dialog (which we never see
        // in headless/off-screen mode), so for authenticated proxies
        // the user may need a proxy-auth extension. For unauthenticated
        // proxies this just works.
        if let Some(p) = crate::transport::proxy::from_env_for("https://ghost.local/") {
            chrome_args.push(format!("--proxy-server={}", p.chrome_proxy_arg()));
        }
        // ── Stealth mode selection ──
        //
        // The goal: run headful Chrome (real GPU, real WebGL, real
        // window.chrome) WITHOUT being visible to the user.
        //
        // Linux: Xvfb virtual display (display = Some(":99")).
        //   Headful on a virtual X display. Zero user-visible artifacts.
        //
        // macOS / Windows: no Xvfb, but headful Chrome with the
        //   window positioned at -32000,-32000 (far off-screen).
        //   The window exists, has real GPU, real WebGL — but the
        //   user never sees it. This is strictly better than
        //   --headless=new, which uses SwiftShader (detectable).
        //
        // Fallback (no display, no platform support): --headless=new.

        #[cfg(linux_like)]
        {
            if let Some(disp) = display {
                // Linux + Xvfb: headful on virtual display.
                cmd.env("DISPLAY", disp);
                chrome_args.push("--ozone-platform=x11".into());
            } else {
                // No Xvfb available (Termux, headless server, WSL
                // without X11). Fall back to headless mode.
                // --headless=new is less stealthy than headful on
                // Xvfb (SwiftShader WebGL, detectable), but it's
                // the only option without a display.
                chrome_args.push("--headless=new".into());
            }
        }

        // Unknown platforms (not Linux/Android/macOS/Windows): headless
        // fallback. Android is covered by linux_like above.
        #[cfg(not(any(linux_like, target_os = "macos", target_os = "windows")))]
        {
            chrome_args.push("--headless=new".into());
        }
        // Modern Chrome (136+) sets navigator.webdriver
        // under --headless/--remote-debugging-port even
        // raw. This blink switch restores the real-
        // browser default; not JS-enumerable.
        chrome_args.push("--disable-blink-features=AutomationControlled".into());
        chrome_args.push("about:blank".into());
        cmd.args(&chrome_args);
        // Own process group (Unix) / Job Object (Windows):
        // freeze/thaw/kill the whole browser tree.
        proc::Proc::prepare_cmd(&mut cmd);
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        // No orphans even if donsetch dies hard. Linux/Android:
        // prctl(PR_SET_PDEATHSIG). macOS has no prctl; Windows
        // uses the Job Object's KILL_ON_JOB_CLOSE.
        #[cfg(linux_like)]
        unsafe {
            cmd.as_std_mut().pre_exec(proc::pdeath_pre_exec);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| FetchError::ghost(format!("spawn: {e}")))?;
        let proc = proc::Proc::from_child(&child)?;

        // The ws endpoint arrives on stderr:
        // "DevTools listening on ws://127.0.0.1:PORT/..."
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| FetchError::ghost("no stderr pipe"))?;
        let mut lines = BufReader::new(stderr).lines();
        let ws_url = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(i) = line.find("ws://") {
                    return Some(line[i..].trim().to_string());
                }
            }
            None
        })
        .await
        .map_err(|_| FetchError::ghost("devtools ws timeout"))?
        .ok_or_else(|| FetchError::ghost("no devtools ws line"))?;

        let cdp = cdp::Cdp::connect(&ws_url).await?;

        // One page target, attached flat.
        let target = cdp
            .call(None, "Target.createTarget", json!({ "url": "about:blank" }))
            .await?
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| FetchError::ghost("no targetId"))?
            .to_string();
        let session = cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target, "flatten": true }),
            )
            .await?
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| FetchError::ghost("no sessionId"))?
            .to_string();
        cdp.call(Some(&session), "Page.enable", json!({})).await?;

        // Stealth JS injection — runs before any page script.
        // Patches the most common automation detection vectors:
        // - navigator.webdriver: belt-and-suspenders alongside
        //   --disable-blink-features=AutomationControlled
        // - navigator.languages: ensure it's set (some Xvfb setups
        //   don't inherit the system locale)
        // - window.chrome: ensure it exists (some headful setups
        //   on Linux miss the chrome.runtime object)
        // - navigator.permissions.query: patch notifications to
        //   return 'denied' (real Chrome default, automation
        //   returns 'prompt' — a known detection vector)
        // - navigator.plugins: ensure length > 0 (headful Chrome
        //   should have plugins, but some setups don't)
        // Minimal patches — over-patching is itself detectable.
        let _ = cdp
            .call(
                Some(&session),
                "Page.addScriptToEvaluateOnNewDocument",
                json!({
                    "source": "\
                        Object.defineProperty(navigator, 'webdriver', { get: () => false });\
                        Object.defineProperty(navigator, 'languages', { get: () => ['en-US', 'en'] });\
                        if (!window.chrome) { window.chrome = {}; }\
                        if (!window.chrome.runtime) { window.chrome.runtime = {}; }\
                        if (navigator.plugins && navigator.plugins.length === 0) {\
                            Object.defineProperty(navigator, 'plugins', { get: () => [{ name: 'Chrome PDF Plugin' }, { name: 'Chrome PDF Viewer' }, { name: 'Native Client' }] });\
                        }\
                    "
                }),
            )
            .await;
        // invisible even on macOS (Dock) and Windows (taskbar).
        // Combined with --window-position=-32000,-32000, the
        // window is both off-screen and minimized. Chrome still
        // renders normally (minimized ≠ background tab; the
        // active tab's visibilityState stays "visible").
        if let Ok(win) = cdp
            .call(
                None,
                "Browser.getWindowForTarget",
                json!({ "targetId": target }),
            )
            .await
            && let Some(id) = win.get("windowId").and_then(Value::as_i64)
        {
            let _ = cdp
                .call(
                    None,
                    "Browser.setWindowBounds",
                    json!({
                        "windowId": id,
                        "bounds": { "windowState": "minimized" }
                    }),
                )
                .await;
        }

        // Unknown platform fallback: headless mode with device
        // metrics override (no real screen geometry available).
        #[cfg(not(any(linux_like, target_os = "macos", target_os = "windows")))]
        {
            cdp.call(
                Some(&session),
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": 1920,
                    "height": 1080,
                    "deviceScaleFactor": 1,
                    "mobile": false,
                    "screenWidth": 1920,
                    "screenHeight": 1080
                }),
            )
            .await?;
        }

        Ok(Self {
            child,
            proc,
            cdp,
            session,
            target,
            frozen: false,
            last_used: Instant::now(),
        })
    }

    #[allow(dead_code)] // useful accessor for debugging/agent surface
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Freeze the whole process tree. CPU → 0, RAM goes
    /// cold and swappable. Resume is ~ms.
    pub fn freeze(&mut self) {
        if self.frozen {
            return;
        }
        self.proc.freeze();
        self.frozen = true;
    }

    /// Resume the process tree. False if the browser died while
    /// frozen (caller relaunches).
    pub fn thaw(&mut self) -> bool {
        if !self.frozen {
            return true;
        }
        match self.child.try_wait() {
            Ok(None) => {
                self.proc.thaw();
                self.frozen = false;
                true
            }
            // Exited (or error) → caller relaunches.
            _ => false,
        }
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Reap the browser entirely — the whole process tree,
    /// plus crashpad handlers on Unix (they daemonize into
    /// their own groups and escape the group kill; on Windows
    /// the Job Object already owns them).
    pub async fn kill(&mut self) {
        self.proc.kill_group();
        sweep_crashpad();
        let _ = self.child.wait().await;
    }

    pub fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    /// Navigate the attached page.
    pub async fn navigate(&self, url: &str) -> Result<(), FetchError> {
        self.cdp
            .call(Some(&self.session), "Page.navigate", json!({ "url": url }))
            .await?;
        Ok(())
    }

    /// Current document HTML. DOM domain only — no Runtime,
    /// no script execution.
    pub async fn outer_html(&self) -> Result<String, FetchError> {
        let root = self
            .cdp
            .call(Some(&self.session), "DOM.getDocument", json!({}))
            .await?
            .get("root")
            .and_then(|r| r.get("nodeId"))
            .and_then(Value::as_i64)
            .ok_or_else(|| FetchError::ghost("no root node"))?;
        Ok(self
            .cdp
            .call(
                Some(&self.session),
                "DOM.getOuterHTML",
                json!({ "nodeId": root }),
            )
            .await?
            .get("outerHTML")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// Current page URL (targetInfo — no Runtime).
    pub async fn current_url(&self) -> Result<String, FetchError> {
        Ok(self
            .cdp
            .call(
                None,
                "Target.getTargetInfo",
                json!({ "targetId": self.target }),
            )
            .await?
            .get("targetInfo")
            .and_then(|t| t.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// All browser cookies with real expiry (browser-level Storage
    /// domain). CDP's `expires` is a Unix timestamp in seconds
    /// (float); -1 or 0 means session cookie → None.
    pub async fn cookies(&self) -> Result<Vec<cache::CookieRecord>, FetchError> {
        let res = self.cdp.call(None, "Storage.getCookies", json!({})).await?;
        let mut out = Vec::new();
        if let Some(arr) = res.get("cookies").and_then(Value::as_array) {
            for c in arr {
                let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                let value = c.get("value").and_then(Value::as_str).unwrap_or("");
                let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
                let expires = c
                    .get("expires")
                    .and_then(|v| v.as_f64())
                    .filter(|&e| e > 0.0)
                    .map(|e| e as u64);
                if !name.is_empty() {
                    out.push(cache::CookieRecord {
                        name: name.to_string(),
                        value: value.to_string(),
                        domain: domain.to_string(),
                        expires_at: expires,
                    });
                }
            }
        }
        Ok(out)
    }

    /// PNG screenshot → path (D16 byproduct).
    pub async fn screenshot(&self, path: &str) -> Result<(), FetchError> {
        let data = self
            .cdp
            .call(
                Some(&self.session),
                "Page.captureScreenshot",
                json!({ "format": "png" }),
            )
            .await?
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| FetchError::ghost("no screenshot data"))?
            .to_string();
        // base64 decode (no new dep: manual).
        let bytes = b64decode(data.as_bytes());
        std::fs::write(path, bytes).map_err(|e| FetchError::ghost(format!("screenshot: {e}")))
    }

    /// One trusted click with a human-ish pre-move path.
    /// CDP input events are isTrusted=true; detection is
    /// behavioral, so the path curves and overshoots.
    pub async fn click(&self, x: f64, y: f64) -> Result<(), FetchError> {
        // Pre-movement: bezier-ish arc from a random offset.
        let mut rng = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let mut rand = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % 1000) as f64 / 1000.0
        };
        let sx = x - 200.0 - rand() * 300.0;
        let sy = y - 100.0 - rand() * 200.0;
        for i in 1..=12 {
            let t = i as f64 / 12.0;
            // Ease-out cubic + slight wobble.
            let e = 1.0 - (1.0 - t).powi(3);
            let wob = (t * 9.0).sin() * 3.0 * (1.0 - t);
            let px = sx + (x - sx) * e + wob;
            let py = sy + (y - sy) * e + wob * 0.6;
            self.cdp
                .call(
                    Some(&self.session),
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": px, "y": py }),
                )
                .await?;
            tokio::time::sleep(std::time::Duration::from_millis(8 + (rand() * 14.0) as u64)).await;
        }
        for ty in ["mousePressed", "mouseReleased"] {
            self.cdp
                .call(
                    Some(&self.session),
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": ty, "x": x, "y": y,
                        "button": "left", "clickCount": 1
                    }),
                )
                .await?;
            tokio::time::sleep(std::time::Duration::from_millis(
                35 + (rand() * 60.0) as u64,
            ))
            .await;
        }
        Ok(())
    }

    /// Move the mouse to (x, y) along the human path WITHOUT
    /// pressing — hover. Reuses the click pre-move geometry.
    pub async fn hover(&self, x: f64, y: f64) -> Result<(), FetchError> {
        let mut rng = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let mut rand = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % 1000) as f64 / 1000.0
        };
        let sx = x - 160.0 - rand() * 240.0;
        let sy = y - 80.0 - rand() * 160.0;
        for i in 1..=10 {
            let t = i as f64 / 10.0;
            let e = 1.0 - (1.0 - t).powi(3);
            let wob = (t * 8.0).sin() * 2.5 * (1.0 - t);
            self.cdp
                .call(
                    Some(&self.session),
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": sx + (x - sx) * e + wob, "y": sy + (y - sy) * e + wob * 0.5 }),
                )
                .await?;
            tokio::time::sleep(std::time::Duration::from_millis(
                10 + (rand() * 16.0) as u64,
            ))
            .await;
        }
        Ok(())
    }

    /// Evaluate a JS expression and return the decoded JSON
    /// result. Caller-invoked Runtime — the same discipline as
    /// the Turnstile geometry lookups in ops.rs: Runtime.enable
    /// is NEVER called, so the DataDome console trap stays
    /// defused. Expression must be an arrow-IIFE returning a
    /// JSON-serializable value.
    pub async fn eval_json(&self, expr: &str) -> Result<Value, FetchError> {
        let res = self
            .cdp
            .call(
                Some(&self.session),
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true, "awaitPromise": false }),
            )
            .await?;
        if let Some(err) = res.get("exceptionDetails") {
            return Err(FetchError::ghost(format!(
                "eval: {}",
                err.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("exception")
            )));
        }
        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Center of the first VISIBLE element matching a CSS
    /// selector, viewport-relative, scrolled into view.
    /// None = no match.
    pub async fn element_center(&self, selector: &str) -> Result<Option<(f64, f64)>, FetchError> {
        let sel = serde_json::to_string(selector).unwrap_or_default();
        let v = self
            .eval_json(&format!(
                "(()=>{{const el=document.querySelector({sel});if(!el)return null;\
                 el.scrollIntoView({{block:'center'}});const r=el.getBoundingClientRect();\
                 return {{x:r.x+r.width/2,y:r.y+r.height/2}};}})()"
            ))
            .await?;
        Ok(parse_point(&v))
    }

    /// Center of the smallest visible element whose OWN text
    /// nodes contain `needle` (button/link by label).
    pub async fn element_center_by_text(
        &self,
        needle: &str,
    ) -> Result<Option<(f64, f64)>, FetchError> {
        let n = serde_json::to_string(needle).unwrap_or_default();
        let v = self
            .eval_json(&format!(
                "(()=>{{const t={n};const w=document.createTreeWalker(document.body,NodeFilter.SHOW_ELEMENT);\
                 let el;while((el=w.nextNode())){{\
                 const own=Array.from(el.childNodes).filter(x=>x.nodeType===3).map(x=>x.textContent).join(' ');\
                 if(own&&own.includes(t)&&(el.offsetParent!==null||el.tagName==='BODY')){{\
                 el.scrollIntoView({{block:'center'}});const r=el.getBoundingClientRect();\
                 return {{x:r.x+r.width/2,y:r.y+r.height/2}};}}}}return null;}})()"
            ))
            .await?;
        Ok(parse_point(&v))
    }

    /// Does the CSS selector match anything? DOM domain only.
    pub async fn selector_exists(&self, selector: &str) -> Result<bool, FetchError> {
        let root = self
            .cdp
            .call(Some(&self.session), "DOM.getDocument", json!({}))
            .await?
            .get("root")
            .and_then(|r| r.get("nodeId"))
            .and_then(Value::as_i64)
            .ok_or_else(|| FetchError::ghost("no root node"))?;
        let node = self
            .cdp
            .call(
                Some(&self.session),
                "DOM.querySelector",
                json!({ "nodeId": root, "selector": selector }),
            )
            .await?
            .get("nodeId")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        Ok(node != 0)
    }

    /// Does the rendered body text contain `needle`?
    pub async fn body_has_text(&self, needle: &str) -> Result<bool, FetchError> {
        let n = serde_json::to_string(needle).unwrap_or_default();
        let v = self
            .eval_json(&format!(
                "!!(document.body&&document.body.innerText.includes({n}))"
            ))
            .await?;
        Ok(v.as_bool().unwrap_or(false))
    }

    /// Type text into the focused element with a human cadence —
    /// log-normal-ish inter-key gaps with rare think-pauses.
    /// CDP key events are isTrusted=true; the cadence is the
    /// behavioral cover (a metronome of exactly-50ms keys is the
    /// tell). ASCII + common Latin-1; non-typable codepoints
    /// fall back to char events.
    pub async fn type_text(&self, text: &str) -> Result<(), FetchError> {
        let mut rng = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ (d.as_millis() as u64) << 12)
            .unwrap_or(0x9e3779b9);
        let mut rand = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % 10_000) as f64 / 10_000.0
        };
        for ch in text.chars() {
            let key = ch.to_string();
            let (code, vk) = key_layout(ch);
            // keyDown (with text → inserts the char) + keyUp.
            self.cdp
                .call(
                    Some(&self.session),
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": "keyDown",
                        "key": key,
                        "code": code,
                        "windowsVirtualKeyCode": vk,
                        "nativeVirtualKeyCode": vk,
                        "text": key,
                    }),
                )
                .await?;
            self.cdp
                .call(
                    Some(&self.session),
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": "keyUp",
                        "key": key,
                        "code": code,
                        "windowsVirtualKeyCode": vk,
                        "nativeVirtualKeyCode": vk,
                    }),
                )
                .await?;
            // Human gap: fast baseline, right-skew tail, 4% pauses.
            let gap = if rand() < 0.04 {
                170.0 + rand() * 150.0
            } else {
                28.0 + rand() * rand() * 140.0
            };
            tokio::time::sleep(std::time::Duration::from_millis(gap as u64)).await;
        }
        Ok(())
    }

    /// Press a named non-printable key: Enter, Tab, Escape,
    /// Backspace, ArrowUp/Down/Left/Right, PageUp/Down, Home, End.
    pub async fn press_key(&self, key: &str) -> Result<(), FetchError> {
        let Some((code, vk)) = named_key(key) else {
            return Err(FetchError::ghost(format!(
                "unknown key {key:?} — supported: Enter, Tab, Escape, Backspace, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, PageUp, PageDown, Home, End"
            )));
        };
        for ty in ["rawKeyDown", "keyUp"] {
            self.cdp
                .call(
                    Some(&self.session),
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": ty,
                        "key": key,
                        "code": code,
                        "windowsVirtualKeyCode": vk,
                        "nativeVirtualKeyCode": vk,
                    }),
                )
                .await?;
            tokio::time::sleep(std::time::Duration::from_millis(45)).await;
        }
        Ok(())
    }

    /// Scroll with trusted mouse-wheel events at the viewport
    /// center. `to`: "top" | "bottom" | "down" — or a pixel
    /// amount via scroll_px. Bottom keeps scrolling until the
    /// page stops growing (lazy-load friendly), bounded.
    pub async fn scroll(&self, to: &str, px: i64) -> Result<(), FetchError> {
        match to {
            "top" => {
                self.eval_json("window.scrollTo(0,0)").await?;
            }
            "bottom" | "down" => {
                let mut last_y = -1i64;
                let mut stall = 0u8;
                for _ in 0..40 {
                    let y = self
                        .eval_json("Math.round(window.scrollY)")
                        .await?
                        .as_i64()
                        .unwrap_or(0);
                    if y == last_y {
                        stall += 1;
                        if stall >= 2 {
                            break; // page stopped moving — done
                        }
                    } else {
                        stall = 0;
                    }
                    last_y = y;
                    self.cdp
                        .call(
                            Some(&self.session),
                            "Input.dispatchMouseEvent",
                            json!({
                                "type": "mouseWheel",
                                "x": 960.0, "y": 540.0,
                                "deltaX": 0, "deltaY": 700,
                            }),
                        )
                        .await?;
                    tokio::time::sleep(std::time::Duration::from_millis(140)).await;
                }
            }
            _ => {
                // Pixel amount, chunked to wheel-sized steps.
                let mut left = px.max(0);
                while left > 0 {
                    let d = left.min(700);
                    self.cdp
                        .call(
                            Some(&self.session),
                            "Input.dispatchMouseEvent",
                            json!({
                                "type": "mouseWheel",
                                "x": 960.0, "y": 540.0,
                                "deltaX": 0, "deltaY": d,
                            }),
                        )
                        .await?;
                    left -= d;
                    tokio::time::sleep(std::time::Duration::from_millis(90)).await;
                }
            }
        }
        // Let scroll-triggered rendering settle.
        tokio::time::sleep(std::time::Duration::from_millis(180)).await;
        Ok(())
    }
}

/// Parse {x, y} from an eval_json result.
fn parse_point(v: &Value) -> Option<(f64, f64)> {
    let x = v.get("x")?.as_f64()?;
    let y = v.get("y")?.as_f64()?;
    Some((x, y))
}

/// (code, windowsVirtualKeyCode) for a printable char.
fn key_layout(ch: char) -> (&'static str, i64) {
    let lower = ch.to_ascii_lowercase();
    match ch {
        'a'..='z' | 'A'..='Z' => {
            let letter = lower.to_ascii_uppercase();
            let code = match letter {
                'A' => "KeyA",
                'B' => "KeyB",
                'C' => "KeyC",
                'D' => "KeyD",
                'E' => "KeyE",
                'F' => "KeyF",
                'G' => "KeyG",
                'H' => "KeyH",
                'I' => "KeyI",
                'J' => "KeyJ",
                'K' => "KeyK",
                'L' => "KeyL",
                'M' => "KeyM",
                'N' => "KeyN",
                'O' => "KeyO",
                'P' => "KeyP",
                'Q' => "KeyQ",
                'R' => "KeyR",
                'S' => "KeyS",
                'T' => "KeyT",
                'U' => "KeyU",
                'V' => "KeyV",
                'W' => "KeyW",
                'X' => "KeyX",
                'Y' => "KeyY",
                _ => "KeyZ",
            };
            (code, letter as i64)
        }
        '0'..='9' => (
            match ch {
                '0' => "Digit0",
                '1' => "Digit1",
                '2' => "Digit2",
                '3' => "Digit3",
                '4' => "Digit4",
                '5' => "Digit5",
                '6' => "Digit6",
                '7' => "Digit7",
                '8' => "Digit8",
                _ => "Digit9",
            },
            ch as i64,
        ),
        ' ' => ("Space", 0x20),
        ',' => ("Comma", 0xBC),
        '.' => ("Period", 0xBE),
        '/' => ("Slash", 0xBF),
        ';' => ("Semicolon", 0xBA),
        '\'' => ("Quote", 0xDE),
        '[' => ("BracketLeft", 0xDB),
        ']' => ("BracketRight", 0xDD),
        '\\' => ("Backslash", 0xDC),
        '-' => ("Minus", 0xBD),
        '=' => ("Equal", 0xBB),
        '`' => ("Backquote", 0xC0),
        '\n' | '\r' => ("Enter", 0x0D),
        '\t' => ("Tab", 0x09),
        _ => ("", 0),
    }
}

/// Named non-printable keys: (code, vk).
fn named_key(key: &str) -> Option<(&'static str, i64)> {
    Some(match key {
        "Enter" | "enter" | "RETURN" => ("Enter", 0x0D),
        "Tab" | "tab" => ("Tab", 0x09),
        "Escape" | "Esc" | "esc" => ("Escape", 0x1B),
        "Backspace" | "backspace" => ("Backspace", 0x08),
        "ArrowUp" | "up" => ("ArrowUp", 0x26),
        "ArrowDown" | "down" => ("ArrowDown", 0x28),
        "ArrowLeft" | "left" => ("ArrowLeft", 0x25),
        "ArrowRight" | "right" => ("ArrowRight", 0x27),
        "PageUp" => ("PageUp", 0x21),
        "PageDown" => ("PageDown", 0x22),
        "Home" => ("Home", 0x24),
        "End" => ("End", 0x23),
        _ => return None,
    })
}

/// Kill chrome_crashpad processes belonging to our
/// ghost profile (they daemonize into their own
/// process groups and escape group kills). Linux-only:
/// uses /proc; macOS has no /proc and Windows's Job
/// Object already owns the crashpad handlers.
#[cfg(linux_like)]
fn sweep_crashpad() {
    let marker = profile_dir().to_string_lossy().into_owned();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read_to_string(e.path().join("cmdline")) else {
            continue;
        };
        if cmdline.contains("crashpad") && cmdline.contains(&marker) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(linux_like))]
fn sweep_crashpad() {}

/// Minimal base64 decode (avoids a dep for one call).
fn b64decode(s: &[u8]) -> Vec<u8> {
    fn val(b: u8) -> u8 {
        match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let clean: Vec<u8> = s
        .iter()
        .copied()
        .filter(|b| !b"=\n\r ".contains(b))
        .collect();
    for chunk in clean.chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let n = ((val(chunk[0]) as u32) << 18)
            | ((val(chunk[1]) as u32) << 12)
            | ((val(chunk[2]) as u32) << 6)
            | (val(chunk[3]) as u32);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
    }
    out
}
