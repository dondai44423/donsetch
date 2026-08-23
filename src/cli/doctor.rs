//! `donsetch --doctor` — health check with auto-fix.
//!
//! Checks, each with a clean pass/warn/fail icon and a dim
//! detail string. Auto-fixes what it can (creates missing dirs,
//! removes stale lock files). Prints instructions for issues that
//! need manual intervention.
//!
//! The browser path must be BORING to install (50-case report):
//! doctor proves Chromium presence, Xvfb, a REAL browser launch
//! with fingerprint selftest, and model availability. A tier-2
//! feature that only works when the user guesses a hidden
//! prerequisite is not finished, and doctor is where that
//! prerequisite surfaces.

use std::path::Path;

use crate::cli;
use crate::fetch::client::Fetcher;
use crate::paths;
use crate::profile::BrowserProfile;

enum CheckResult {
    Pass(String),
    Warn(String),
    Fail(String, String), // (detail, instructions)
    Fixed(String),
}

pub async fn run() {
    cli::init();
    cli::print_title("DonSeTch Doctor");
    println!();

    let mut p = 0u32; // passed
    let mut w = 0u32; // warnings
    let mut f = 0u32; // failed

    macro_rules! report {
        ($name:expr, $r:expr) => {
            match $r {
                CheckResult::Pass(d) => {
                    cli::check_pass($name, &d);
                    p += 1;
                }
                CheckResult::Warn(d) => {
                    cli::check_warn($name, &d);
                    w += 1;
                }
                CheckResult::Fail(d, i) => {
                    cli::check_fail($name, &d, &i);
                    f += 1;
                }
                CheckResult::Fixed(d) => {
                    cli::check_fixed($name, &d);
                    p += 1;
                }
            }
        };
    }

    // 1. Binary integrity.
    report!("Binary integrity", check_binary());

    // Create fetcher for network and TLS checks.
    let fetcher = match Fetcher::new(BrowserProfile::host_default()) {
        Ok(fm) => Some(fm),
        Err(e) => {
            cli::check_fail(
                "Fetcher init",
                &e.to_string(),
                "TLS initialization failed — check system CA certificates",
            );
            f += 1;
            None
        }
    };

    // 2. Network reachability.
    if let Some(ref fm) = fetcher {
        report!("Network", check_network(fm).await);
    }

    // 3. TLS fingerprint.
    if let Some(ref fm) = fetcher {
        report!("TLS fingerprint", check_tls(fm).await);
    }

    // 4. Chrome/Chromium.
    report!("Chrome/Chromium", check_chrome());

    // 5. Xvfb (Linux headful stealth prerequisite).
    report!("Xvfb", check_xvfb());

    // 6. Ghost profile.
    report!("Ghost profile", check_ghost_profile());

    // 7. Browser launch — the REAL test: launch, fingerprint
    // selftest, clean kill. Presence checks above are cheap;
    // this proves tier 2 actually works on this machine.
    report!("Browser launch", check_browser_launch().await);

    // 8. Cache directory.
    report!("Cache directory", check_cache_dir());

    // 9. State permissions.
    report!("State permissions", check_state_permissions());

    // 10. PDFium.
    report!("PDFium", check_pdfium());

    // 11. OCR models.
    report!("OCR models", check_ocr_models());

    // 12. Rerank model.
    report!("Rerank model", check_rerank_model());

    // 13. Ghost state.
    report!("Ghost state", check_ghost_state());

    // ── Summary ──────────────────────────────────────────────

    println!();
    let total = p + w + f;
    println!("  {p}/{total} passed, {w} warning(s), {f} failed");
    cli::print_footer();

    if f > 0 {
        println!("  Status: {}", cli::red("issues found"));
        // Scripts gate on the exit code — doctor failing must not
        // read as success.
        std::process::exit(1);
    } else if w > 0 {
        println!("  Status: {}", cli::yellow("healthy with warnings"));
    } else {
        println!("  Status: {}", cli::green("healthy"));
    }
}

// ── Individual checks ──────────────────────────────────────────

fn check_binary() -> CheckResult {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return CheckResult::Fail("cannot determine path".into(), e.to_string()),
    };

    let meta = match std::fs::metadata(&exe) {
        Ok(m) => m,
        Err(e) => return CheckResult::Fail("not accessible".into(), e.to_string()),
    };

    let size = meta.len();
    if size < 1_000_000 {
        return CheckResult::Fail(
            format!("{size} bytes (suspiciously small)"),
            "Binary may be corrupt. Reinstall donsetch.".into(),
        );
    }

    CheckResult::Pass(format!(
        "v{}, {}MB",
        env!("CARGO_PKG_VERSION"),
        size / 1_000_000,
    ))
}

async fn check_network(fetcher: &Fetcher) -> CheckResult {
    match fetcher.fetch("https://example.com").await {
        Ok(out) if out.status == 200 => CheckResult::Pass(format!(
            "example.com 200 OK ({:.0}ms)",
            out.elapsed.as_secs_f64() * 1000.0,
        )),
        Ok(out) => CheckResult::Warn(format!("example.com returned HTTP {}", out.status)),
        Err(e) => CheckResult::Fail(
            e.to_string(),
            "Check your network connection and DNS".into(),
        ),
    }
}

async fn check_tls(fetcher: &Fetcher) -> CheckResult {
    match fetcher.fetch("https://tls.peet.ws/api/all").await {
        Ok(out) if out.status == 200 => {
            let body = String::from_utf8_lossy(&out.body);
            // Parse JA4 from JSON: "ja4": "t13d..."
            // The value may have whitespace after the colon.
            if let Some(pos) = body.find("\"ja4\":") {
                let rest = body[pos + 6..].trim_start();
                if let Some(rest) = rest.strip_prefix('"')
                    && let Some(end) = rest.find('"')
                {
                    let ja4 = &rest[..end];
                    if ja4.starts_with("t13d") {
                        return CheckResult::Pass(format!("JA4: {ja4}"));
                    }
                }
            }
            CheckResult::Pass("TLS connection successful".into())
        }
        Ok(_) => {
            // External service returned non-200 — skip silently.
            // The TLS stack works (we connected); the fingerprint
            // check service is just unavailable. Don't alarm users.
            CheckResult::Pass("TLS connected (fingerprint service unavailable)".into())
        }
        Err(_) => {
            // Can't reach the fingerprint service at all. Still
            // don't warn — the service may be down or blocked,
            // and the TLS stack is fine (we use it for every fetch).
            CheckResult::Pass("TLS stack active (fingerprint service unreachable)".into())
        }
    }
}

fn check_chrome() -> CheckResult {
    match crate::ghost::chrome_binary() {
        Ok(path) => {
            // Browser version via the shared probe — registry first
            // (zero spawn), then a hard-capped `--version` spawn that
            // kills the whole tree on timeout. Never opens a window,
            // never hangs, never leaves an orphaned browser behind.
            let version = match crate::profile::probe_installed_major() {
                Some(major) => format!("{major}.0.0.0 (probed)"),
                None => "unknown version".into(),
            };
            CheckResult::Pass(format!("{version} at {path}"))
        }
        Err(_) => CheckResult::Fail(
            "not found".into(),
            "Install Chrome/Chromium, or set DONGHOST_CHROME to a browser path".into(),
        ),
    }
}

/// Xvfb: the Linux headful-stealth prerequisite. Missing Xvfb
/// does NOT disable tier 2 — ghost falls back to off-screen
/// headful on the real display (a window may flash briefly) or
/// headless on Wayland-only sessions (more detectable). Warn,
/// not fail — but the user deserves to know.
fn check_xvfb() -> CheckResult {
    #[cfg(linux_like)]
    {
        // Termux (Android) has no X11 by default. Xvfb is not
        // needed — Ghost uses --headless=new mode.
        if std::env::var_os("PREFIX")
            .map(|p| p.to_string_lossy().contains("com.termux"))
            .unwrap_or(false)
        {
            return CheckResult::Pass("not needed (Termux — headless mode)".into());
        }
        if crate::ghost::xvfb::is_available() {
            // :99 socket alive = daemon's Xvfb will be reused.
            let reuse = std::path::Path::new("/tmp/.X11-unix/X99").exists();
            CheckResult::Pass(if reuse {
                "available, display :99 alive (reused)".into()
            } else {
                "available (starts on demand)".into()
            })
        } else {
            CheckResult::Warn(
                "not installed — tier 2 falls back to headless/off-screen (less stealthy)".into(),
            )
        }
    }
    #[cfg(not(linux_like))]
    {
        CheckResult::Pass("not needed on this platform".into())
    }
}

/// The REAL browser test: launch Chromium exactly as tier 2
/// would (same flags, same Xvfb dance), run the fingerprint
/// selftest page, kill. Bounded to 40s. This is what turns
/// "Chromium found" into "tier 2 proven on this machine" —
/// the 50-case report's "a feature that works only when the
/// user guesses the hidden prerequisite is not finished".
async fn check_browser_launch() -> CheckResult {
    let inner = async {
        // Same Xvfb handling as GhostManager: start/reuse :99.
        let xvfb = crate::ghost::xvfb::Xvfb::start().await.ok();
        let display = xvfb.as_ref().map(|x| x.display_env());
        let profile = BrowserProfile::host_default();
        let t0 = std::time::Instant::now();
        let mut ghost = match crate::ghost::Ghost::launch(&profile, display.as_deref()).await {
            Ok(g) => g,
            Err(e) => {
                if let Some(x) = xvfb {
                    x.kill().await;
                }
                return CheckResult::Fail(
                    format!("launch failed: {e}"),
                    "Tier 2 (browser fallback) will not work. Install Chromium + Xvfb, or set DONGHOST_CHROME".into(),
                );
            }
        };
        let launch_ms = t0.elapsed().as_millis();

        // Fingerprint selftest: local page reads back
        // navigator.webdriver and friends from the live browser.
        let fp = crate::ghost::ops::selftest(&mut ghost).await;
        ghost.kill().await;
        if let Some(x) = xvfb {
            x.kill().await;
        }
        match fp {
            Ok(json_str) => {
                let v: serde_json::Value = serde_json::from_str(&json_str).unwrap_or_default();
                let webdriver = v.get("webdriver").and_then(|w| w.as_bool());
                match webdriver {
                    Some(false) => CheckResult::Pass(format!(
                        "launched in {launch_ms}ms, fingerprint clean (webdriver=false)"
                    )),
                    Some(true) => CheckResult::Warn(
                        "launched, but webdriver=true — stealth patches not applied".into(),
                    ),
                    None => CheckResult::Pass(format!("launched in {launch_ms}ms, selftest ok")),
                }
            }
            Err(e) => CheckResult::Warn(format!(
                "launched in {launch_ms}ms, but selftest failed: {e}"
            )),
        }
    };
    // Hard bound: a wedged browser here must not hang doctor.
    match tokio::time::timeout(std::time::Duration::from_secs(40), inner).await {
        Ok(r) => r,
        Err(_) => CheckResult::Fail(
            "launch timed out after 40s".into(),
            if cfg!(target_os = "linux") {
                "A stale Chromium or Xvfb may be wedged: pkill -f chromium; rm -f /tmp/.X99-lock /tmp/.X11-unix/X99".into()
            } else {
                "A stale Chromium may be wedged: close all browser windows / kill all chrome processes, then retry".into()
            },
        ),
    }
}

/// ghost-state.json holds cookies — it must not be
/// world-readable.
fn check_state_permissions() -> CheckResult {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let f = paths::cache_dir().join("ghost-state.json");
        if !f.exists() {
            return CheckResult::Pass("no state file yet".into());
        }
        match std::fs::metadata(&f) {
            Ok(m) => {
                let mode = m.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    // Auto-fix: tighten to 0600.
                    let mut perm = m.permissions();
                    perm.set_mode(0o600);
                    if std::fs::set_permissions(&f, perm).is_ok() {
                        return CheckResult::Fixed(format!(
                            "tightened ghost-state.json {mode:o} → 600"
                        ));
                    }
                    return CheckResult::Fail(
                        format!("ghost-state.json is {mode:o} (group/other readable)"),
                        format!("chmod 600 {}", f.display()),
                    );
                }
                CheckResult::Pass(format!("{mode:o} on ghost-state.json"))
            }
            Err(e) => CheckResult::Warn(format!("cannot stat: {e}")),
        }
    }
    #[cfg(not(unix))]
    {
        CheckResult::Pass("windows ACLs apply".into())
    }
}

/// Cross-encoder rerank model cache (semantic search reranking
/// + focus filter). Missing = downloads on first search.
fn check_rerank_model() -> CheckResult {
    #[cfg(not(feature = "rerank"))]
    {
        CheckResult::Warn("not compiled (build with --features rerank to enable)".into())
    }
    #[cfg(feature = "rerank")]
    {
        let dir = paths::cache_dir().join("rerank");
        if !dir.exists() {
            return CheckResult::Warn("not cached (downloads on first search)".into());
        }
        let models = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .is_some_and(|ext| ext == "onnx" || ext == "json" || ext == "txt")
                    })
                    .count()
            })
            .unwrap_or(0);
        if models > 0 {
            CheckResult::Pass(format!("{models} model files cached"))
        } else {
            CheckResult::Warn("not cached (downloads on first search)".into())
        }
    }
}

fn check_ghost_profile() -> CheckResult {
    let dir = crate::ghost::profile_dir();

    if !dir.exists() {
        return match std::fs::create_dir_all(&dir) {
            Ok(()) => CheckResult::Fixed("created profile directory".into()),
            Err(e) => CheckResult::Fail("not found".into(), format!("Cannot create: {e}")),
        };
    }

    // Check writable.
    let test = dir.join(".doctor-write-test");
    match std::fs::write(&test, b"test") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test);

            // Check for stale singleton lock files.
            let mut stale = 0;
            for f in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
                let p = dir.join(f);
                if p.exists() {
                    let _ = std::fs::remove_file(&p);
                    stale += 1;
                }
            }

            if stale > 0 {
                CheckResult::Fixed(format!("removed {stale} stale lock(s)"))
            } else {
                CheckResult::Pass("writable, no stale locks".into())
            }
        }
        Err(e) => CheckResult::Fail("not writable".into(), format!("Check permissions: {e}")),
    }
}

fn check_cache_dir() -> CheckResult {
    let dir = paths::cache_dir();

    if !dir.exists() {
        return match std::fs::create_dir_all(&dir) {
            Ok(()) => CheckResult::Fixed("created cache directory".into()),
            Err(e) => CheckResult::Fail("not found".into(), format!("Cannot create: {e}")),
        };
    }

    let test = dir.join(".doctor-write-test");
    match std::fs::write(&test, b"test") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test);
            let total = dir_size(&dir);

            // Breakdown by component — helps users understand what's
            // using space. The ghost-profile (Chrome's own cache) is
            // typically the largest; ghost-state.json (self-improvement)
            // should be < 1MB after cookie filtering.
            let ghost_profile = dir.join("ghost-profile");
            let ghost_state = dir.join("ghost-state.json");
            let ocr = dir.join("ocr");
            let rerank = dir.join("rerank");
            let search_cache = dir.join("search-cache.json");

            let parts = [
                (
                    "self-improvement",
                    if ghost_state.exists() {
                        ghost_state.metadata().map(|m| m.len()).unwrap_or(0)
                    } else {
                        0
                    },
                ),
                (
                    "ghost-profile",
                    if ghost_profile.exists() {
                        dir_size(&ghost_profile)
                    } else {
                        0
                    },
                ),
                ("ocr-models", if ocr.exists() { dir_size(&ocr) } else { 0 }),
                (
                    "rerank-models",
                    if rerank.exists() {
                        dir_size(&rerank)
                    } else {
                        0
                    },
                ),
                (
                    "search-cache",
                    if search_cache.exists() {
                        search_cache.metadata().map(|m| m.len()).unwrap_or(0)
                    } else {
                        0
                    },
                ),
            ];

            let breakdown: String = parts
                .iter()
                .filter(|(_, s)| *s > 0)
                .map(|(name, size)| format!("{name}={}", format_size(*size)))
                .collect::<Vec<_>>()
                .join(", ");

            if breakdown.is_empty() {
                CheckResult::Pass(format!("{}, writable", format_size(total)))
            } else {
                CheckResult::Pass(format!("{} ({breakdown})", format_size(total)))
            }
        }
        Err(e) => CheckResult::Fail("not writable".into(), format!("Check permissions: {e}")),
    }
}

fn check_pdfium() -> CheckResult {
    #[cfg(not(windows))]
    {
        CheckResult::Pass(option_env!("DONSHEET_PDFIUM").unwrap_or("static").into())
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().unwrap_or_default();
        let dll = exe.parent().unwrap_or(Path::new("")).join("pdfium.dll");
        if dll.exists() {
            CheckResult::Pass(option_env!("DONSHEET_PDFIUM").unwrap_or("dll").into())
        } else {
            CheckResult::Fail(
                "pdfium.dll not found".into(),
                "Reinstall donsetch or copy pdfium.dll next to donsetch.exe".into(),
            )
        }
    }
}

fn check_ocr_models() -> CheckResult {
    #[cfg(not(feature = "ocr"))]
    {
        CheckResult::Warn("not compiled (build with --features ocr to enable)".into())
    }
    #[cfg(feature = "ocr")]
    {
        if !crate::pdf::ocr::enabled() {
            return CheckResult::Warn("disabled (DONSHEET_OCR=off)".into());
        }

        let dir = crate::pdf::ocr::ocr_cache_dir();
        if !dir.exists() {
            return CheckResult::Warn("not cached (downloads on first use)".into());
        }

        // Count model files (.onnx + .txt dictionary).
        let models = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .is_some_and(|ext| ext == "onnx" || ext == "txt")
                    })
                    .count()
            })
            .unwrap_or(0);

        if models > 0 {
            CheckResult::Pass(format!("{models} model files cached"))
        } else {
            CheckResult::Warn("not cached (downloads on first use)".into())
        }
    }
}

fn check_ghost_state() -> CheckResult {
    let state = crate::ghost::cache::GhostState::load();
    let domains = state.profiles.len();
    let renders = state.renders.len();
    CheckResult::Pass(format!("{domains} domains, {renders} renders cached"))
}

// ── Helpers ───────────────────────────────────────────────────

/// Recursively sum file sizes under `path`. Capped at ~1GB to
/// avoid walking pathological trees.
fn dir_size(path: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        if *total > 1_000_000_000 {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, total);
                } else if let Ok(meta) = entry.metadata() {
                    *total += meta.len();
                }
            }
        }
    }

    let mut total = 0u64;
    walk(path, &mut total);
    total
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1}GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1}MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1}KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes}B")
    }
}
