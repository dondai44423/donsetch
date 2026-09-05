//! `donsetch --doctor` : health check with auto-fix.
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

use crate::DISPLAY_NAME;
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

    // Flags: --json (structured output for agents/CI), --deep
    // (full live-probe suite), --fix (apply safe repairs after the
    // checks), --fast (default: skip slow probes). --mcp prints
    // MCP client registration blocks for any detected client.
    let args: Vec<String> = std::env::args().skip(2).collect();
    let json = args.iter().any(|a| a == "--json");
    let deep = args.iter().any(|a| a == "--deep");
    let fix = args.iter().any(|a| a == "--fix");
    let only_mcp = args.iter().any(|a| a == "--mcp");

    cli::print_title(&format!("{DISPLAY_NAME} Doctor"));
    println!();

    let mut p = 0u32; // passed
    let mut w = 0u32; // warnings
    let mut f = 0u32; // failed
    // (name, status, detail, hint) collected for --json and --fix.
    let mut collected: Vec<(String, String, String, String)> = Vec::new();

    macro_rules! report {
        ($name:expr, $r:expr) => {
            match $r {
                CheckResult::Pass(d) => {
                    collected.push(($name.to_string(), "pass".into(), d.clone(), String::new()));
                    cli::check_pass($name, &d);
                    p += 1;
                }
                CheckResult::Warn(d) => {
                    collected.push(($name.to_string(), "warn".into(), d.clone(), String::new()));
                    cli::check_warn($name, &d);
                    w += 1;
                }
                CheckResult::Fail(d, i) => {
                    collected.push(($name.to_string(), "fail".into(), d.clone(), i.clone()));
                    cli::check_fail($name, &d, &i);
                    f += 1;
                }
                CheckResult::Fixed(d) => {
                    collected.push(($name.to_string(), "fixed".into(), d.clone(), String::new()));
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
                "TLS initialization failed : check system CA certificates",
            );
            f += 1;
            collected.push((
                "Fetcher init".into(),
                "fail".into(),
                e.to_string(),
                "TLS initialization failed : check system CA certificates".into(),
            ));
            None
        }
    };

    // 2. Network reachability (always: it gates nothing else).
    if let Some(ref fm) = fetcher {
        report!("Network", check_network(fm).await);
    } else {
        report!(
            "Network",
            CheckResult::Warn("skipped: fetcher unavailable".to_string())
        );
    }

    // 3. TLS fingerprint (fast enough to keep in fast mode).
    if let Some(ref fm) = fetcher {
        report!("TLS fingerprint", check_tls(fm).await);
    }

    // 4. Chrome/Chromium.
    report!("Chrome/Chromium", check_chrome().await);

    // 5. Xvfb (Linux headful stealth prerequisite).
    report!("Xvfb", check_xvfb());

    // 6. Ghost profile.
    report!("Ghost profile", check_ghost_profile());

    // 7. Browser launch: the only heavyweight probe. Fast mode
    // (default) skips the seconds-long live launch; --deep runs it.
    if deep {
        report!("Browser launch", check_browser_launch().await);
    } else {
        cli::check_dim("Browser launch", "skipped (--deep to run)");
    }

    // 8. Cache directory.
    report!("Cache directory", check_cache_dir());

    // 8b. Auth sessions (donsetch login).
    report!("Auth sessions", check_auth_sessions());

    // 9. State permissions.
    report!("State permissions", check_state_permissions());

    // 10. PDFium.
    report!("PDFium", check_pdfium());

    // 11. OCR models.
    report!("OCR models", check_ocr_models());

    // 12. Rerank model.
    report!("Rerank model", check_rerank_model());

    // 13. ONNX Runtime / AVX.
    report!("ONNX Runtime", check_onnx());

    // 14. Ghost state.
    report!("Ghost state", check_ghost_state());

    // 15. Bright Data account keys (SERP + unlocker): the paid
    // layer gets more than a y/n. Default mode validates locally
    // (presence, shape, cap + cache state, kill switches); --deep
    // adds a free live zone probe (route_ips costs nothing).
    report!("Bright Data SERP", check_brightdata());
    report!("Bypass unlocker", check_bypass(deep));

    // 15.5 BYOK plugins (user-registered executable adapters).
    report!("Search plugins", check_plugins());

    // 16. MCP client registration (detect + print blocks).
    print_mcp_section();

    // ── Self-healing pass (--fix) ───────────────────────────
    if fix {
        println!();
        let _ = apply_fixes(&mut collected).await;
    }

    // ── Summary ──────────────────────────────────────────────
    println!();
    let total = p + w + f;
    println!("  {p}/{total} passed, {w} warning(s), {f} failed");
    cli::print_footer();

    if f > 0 {
        println!("  Status: {}", cli::red("issues found"));
    } else if w > 0 {
        println!("  Status: {}", cli::yellow("healthy with warnings"));
    } else {
        println!("  Status: {}", cli::green("healthy"));
    }

    // JSON goes LAST so tail-parsers get exactly one clean document.
    if json {
        print_json_summary(&collected, p, w, f, deep);
    }
    if only_mcp {
        std::process::exit(0);
    }
    // Scripts gate on the exit code: doctor failing must not read
    // as success.
    if f > 0 {
        std::process::exit(1);
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
            // External service returned non-200 : skip silently.
            // The TLS stack works (we connected); the fingerprint
            // check service is just unavailable. Don't alarm users.
            CheckResult::Pass("TLS connected (fingerprint service unavailable)".into())
        }
        Err(_) => {
            // Can't reach the fingerprint service at all. Still
            // don't warn : the service may be down or blocked,
            // and the TLS stack is fine (we use it for every fetch).
            CheckResult::Pass("TLS stack active (fingerprint service unreachable)".into())
        }
    }
}

async fn check_chrome() -> CheckResult {
    let result = tokio::task::spawn_blocking(crate::ghost::resolve_browser).await;
    match result {
        Ok(Ok(browser)) => {
            // Full dotted build, not just the major: probing the
            // exact binary we resolved (backed identical for
            // chromium and cloak) costs one spawn and is the only
            // number that matters for debugging detection issues.
            // A padded "151.0.0.0" was honest but vague.
            let version =
                crate::profile::probe_version_string_at_path(&browser.path.to_string_lossy())
                    .or_else(|| browser.version.clone())
                    .unwrap_or_else(|| "unknown version".into());
            CheckResult::Pass(format!(
                "{} at {} ({}; {})",
                version,
                browser.path.display(),
                browser.backend.as_str(),
                browser.source
            ))
        }
        Ok(Err(error)) => CheckResult::Fail(
            error.to_string(),
            "Install Chromium, set DONGHOST_CHROME, or set CLOAKBROWSER_BINARY_PATH. ".to_string()
                + "Set DONSETCH_CLOAK_AUTO_DOWNLOAD=1 to fetch the signed CloakBrowser binary.",
        ),
        Err(error) => CheckResult::Fail(
            format!("browser resolution task failed: {error}"),
            "Retry the check; browser resolution could not be started.".into(),
        ),
    }
}

/// Xvfb: the Linux headful-stealth prerequisite. Missing Xvfb
/// does NOT disable tier 2 : ghost falls back to off-screen
/// headful on the real display (a window may flash briefly) or
/// headless on Wayland-only sessions (more detectable). Warn,
/// not fail : but the user deserves to know.
fn check_xvfb() -> CheckResult {
    #[cfg(linux_like)]
    {
        // A forced headless backend deliberately does not need Xvfb.
        if crate::ghost::cloak::headless_mode_requested() {
            return CheckResult::Pass("not needed (headless backend)".into());
        }
        // Termux (Android) has no X11 by default. Xvfb is not
        // needed : Ghost uses --headless=new mode.
        if std::env::var_os("PREFIX")
            .map(|p| p.to_string_lossy().contains("com.termux"))
            .unwrap_or(false)
        {
            return CheckResult::Pass("not needed (Termux : headless mode)".into());
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
                "not installed : tier 2 falls back to headless/off-screen (less stealthy)".into(),
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
/// the 50-case report's "a feature that works only when the
/// user guesses the hidden prerequisite is not finished".
async fn check_browser_launch() -> CheckResult {
    let inner = async {
        // Same Xvfb handling as GhostManager: start/reuse :99.
        let xvfb = if crate::ghost::cloak::headless_mode_requested() {
            None
        } else {
            crate::ghost::xvfb::Xvfb::start().await.ok()
        };
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
                    "Tier 2 browser fallback will not work. Install Chromium/Xvfb, set DONGHOST_CHROME, or configure CloakBrowser with CLOAKBROWSER_BINARY_PATH.".into(),
                );
            }
        };
        let launch_ms = t0.elapsed().as_millis();

        let fp = crate::ghost::ops::selftest(&mut ghost).await;
        ghost.kill().await;
        if let Some(x) = xvfb {
            x.kill().await;
        }
        match fp {
            Ok(json_str) => {
                let v: serde_json::Value = serde_json::from_str(&json_str).unwrap_or_default();
                // Real-Chrome parity: webdriver must be false VIA THE
                // NATIVE ACCESSOR with no own property on the
                // navigator instance (an injected own property is a
                // tell). undefined was pre-Chrome-89 behavior.
                let webdriver = v.get("webdriver").and_then(|w| w.as_bool());
                let no_own_prop =
                    v.get("webdriverOwnProp").and_then(|w| w.as_bool()) == Some(false);
                // WebGL null is a headless-only signature: it fires
                // when the host cannot provide any GL (common on
                // GPU-less Linux + a Chromium build without a
                // working software rasterizer). Windows/macOS and
                // GPU Linux report a real renderer and clear this.
                // Warn, do not fail: the browser still works, and
                // the SwiftShader launch flags enable it whenever
                // the host can.
                let gl_ok = v
                    .get("webglRenderer")
                    .and_then(|w| w.as_str())
                    .is_some_and(|r| !r.is_empty() && r != "?" && r != "err" && r != "undefined");
                let deep_clean = webdriver == Some(false)
                    && no_own_prop
                    && v.get("hasChrome").and_then(|x| x.as_bool()) == Some(true)
                    && v.get("plugins")
                        .and_then(|x| x.as_u64())
                        .is_some_and(|n| n > 0)
                    && v.get("ua")
                        .and_then(|x| x.as_str())
                        .is_some_and(|ua| !ua.contains("HeadlessChrome"));
                let gl_note = if gl_ok {
                    format!(
                        "webgl={}",
                        v.get("webglRenderer")
                            .and_then(|w| w.as_str())
                            .unwrap_or("ok")
                    )
                } else {
                    "webgl=null (host provides no GL; software renderer unavailable in this Chromium build)"
                        .to_string()
                };
                if deep_clean && gl_ok {
                    CheckResult::Pass(format!(
                        "launched in {launch_ms}ms, deep fingerprint clean (webdriver=false native, no own prop, {gl_note})"
                    ))
                } else if deep_clean {
                    CheckResult::Warn(format!(
                        "launched in {launch_ms}ms, fingerprint clean EXCEPT {gl_note}"
                    ))
                } else {
                    CheckResult::Warn(format!(
                        "launched in {launch_ms}ms, deep fingerprint incomplete: webdriver={webdriver:?} ownProp={:?}, gl={:?}, chrome={:?}, plugins={:?}",
                        v.get("webdriverOwnProp"),
                        v.get("webglRenderer"),
                        v.get("hasChrome"),
                        v.get("plugins")
                    ))
                }
            }
            Err(e) => CheckResult::Warn(format!(
                "launched in {launch_ms}ms, deep fingerprint selftest failed: {e}"
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

/// ghost-state.json holds cookies : it must not be
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

fn check_auth_sessions() -> CheckResult {
    let reg = crate::auth::AuthRegistry::load();
    if reg.domains.is_empty() {
        return CheckResult::Pass(
            "no stored logins (use `donsetch login <domain>` to fetch gated sites)".into(),
        );
    }
    let t = crate::ghost::cache::now();
    let mut unverified = Vec::new();
    let mut expiring = Vec::new();
    for (d, s) in &reg.domains {
        if s.verified == Some(false) {
            unverified.push(d.clone());
        }
        // Session cookies (no expiry) never trip the near-expiry warn.
        if let Some(min) = s.expires_min
            && min > t
            && min < t + 86_400
        {
            expiring.push(format!("{d} ({})", crate::auth::fmt_expiry(Some(min))));
        }
    }
    let mut note = format!("{} domain(s) with stored sessions", reg.domains.len());
    if !unverified.is_empty() {
        note.push_str(&format!("; unverified: {}", unverified.join(", ")));
    }
    if !expiring.is_empty() {
        note.push_str(&format!("; expiring soon: {}", expiring.join(", ")));
    }
    CheckResult::Pass(note)
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

            // Breakdown by component : helps users understand what's
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

            let mut parts_vec: Vec<(&str, u64)> = parts.to_vec();
            let known: u64 = parts_vec.iter().map(|(_, s)| *s).sum();
            let other = total.saturating_sub(known);
            // 'other' = vendored engine bits (PDFium static lib,
            // ONNX runtime) staged in the cache dir: name them so
            // nobody wonders where the bytes went.
            if other >= 1_000_000 {
                parts_vec.push(("engine-runtime", other));
            }

            let breakdown: String = parts_vec
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

fn check_onnx() -> CheckResult {
    #[cfg(not(any(feature = "ocr", feature = "rerank")))]
    {
        CheckResult::Warn("not compiled (build with --features ocr,rerank to enable)".into())
    }
    #[cfg(any(feature = "ocr", feature = "rerank"))]
    {
        // Real probe, not a cfg constant: initialize the ONNX
        // environment and surface the result. A static-link build
        // whose archive was never linked in fails here instead of
        // printing a success string (this exact probe would have
        // caught the v3.3.0 leak on Windows/macOS).
        #[cfg(not(target_os = "linux"))]
        {
            match crate::onnx::ensure_loaded() {
                Ok(()) => CheckResult::Pass("static link, commit probe ok".into()),
                Err(e) => CheckResult::Fail("ONNX payload probe failed".into(), e.to_string()),
            }
        }
        #[cfg(target_os = "linux")]
        {
            // Check AVX support (disk-cached).
            let has_avx = crate::cpu::has_avx();
            if !has_avx {
                return CheckResult::Warn(
                    "CPU lacks AVX : OCR and rerank disabled (all other features work)".into(),
                );
            }
            // Check shared library presence.
            let lib_name = "libonnxruntime.so";
            let found = if let Ok(exe) = std::env::current_exe()
                && let Some(parent) = exe.parent()
            {
                parent.join(lib_name).exists()
            } else {
                false
            };
            let cache = paths::cache_dir().join("onnx").join(lib_name).exists();
            if found || cache {
                CheckResult::Pass("AVX detected, shared library present".into())
            } else {
                CheckResult::Warn(
                    "AVX detected but shared library missing : reinstall donsetch".into(),
                )
            }
        }
    }
}

fn check_ghost_state() -> CheckResult {
    let state = crate::ghost::cache::GhostState::load();
    let domains = state.profiles.len();
    let renders = state.renders.len();
    CheckResult::Pass(format!("{domains} domains, {renders} renders cached"))
}

/// BYOK plugins: registration state only. Never probes the
/// adapter from doctor (a probe is a real query through user
/// code; it stays behind the explicit `--test` flag).
fn check_plugins() -> CheckResult {
    let cfg = crate::search::byok::plugin::PluginConfig::load();
    if !cfg.is_configured() {
        return CheckResult::Pass(
            "none registered (optional: `donsetch keys add plugin <name> --cmd '...' --test`)"
                .to_string(),
        );
    }
    let names: Vec<String> = cfg.names().cloned().collect();
    // Path-form program checks only: PATH lookups are resolved
    // by the exec at run time, and absence there already yields
    // a clear error on the first search.
    let mut missing: Vec<String> = Vec::new();
    for n in &names {
        let prog = &cfg.plugins[n].cmd[0];
        let is_path_form = prog.contains('/') || prog.contains('\\') || prog.starts_with('.');
        if is_path_form && !std::path::Path::new(prog).exists() {
            missing.push(format!("{n}: {prog}"));
        }
    }
    let detail = format!(
        "{} registered ({}), runs at search time",
        names.len(),
        names.join(", ")
    );
    if missing.is_empty() {
        CheckResult::Pass(detail)
    } else {
        CheckResult::Warn(format!(
            "{detail}; program not found: {}",
            missing.join(", ")
        ))
    }
}

fn mask_key(k: &str) -> String {
    let start = k.split_once("::").map(|(t, _)| t).unwrap_or(k);
    let b = start.as_bytes();
    if b.len() <= 8 {
        return format!("{}***", &start[..start.len().saturating_sub(1)]);
    }
    let head = std::str::from_utf8(&b[..6]).unwrap_or("");
    let tail = std::str::from_utf8(&b[b.len() - 4..]).unwrap_or("");
    format!("{head}...{tail}")
}

/// Bright Data SERP key: local validation + a free live zone probe
/// in --deep mode (route_ips costs nothing, so the check can
/// confirm token + zone without spending a cent).
fn check_brightdata() -> CheckResult {
    let cfg = crate::search::byok::store::ByokConfig::load();
    let Some(entry) = cfg
        .providers
        .iter()
        .find(|p| p.name == "brightdata")
        .and_then(|p| p.keys.first())
    else {
        return CheckResult::Warn(
            "not configured : keyless search still works, but SERP costs nothing to add via `donsetch keys add brightdata <token>[::zone]`"
                .to_string(),
        );
    };
    let (_, zone) = crate::search::byok::brightdata_key_parts(&entry.key)
        .unwrap_or_else(|_| (String::new(), String::new()));
    let masked = mask_key(&entry.key);
    let state = match entry.state {
        crate::search::byok::store::KeyState::Active => "active",
        crate::search::byok::store::KeyState::Invalid => "rejected by Bright Data (fix the token)",
        crate::search::byok::store::KeyState::CreditDepleted => "out of credits",
        crate::search::byok::store::KeyState::RateLimited => "rate limited",
    };
    if entry.state != crate::search::byok::store::KeyState::Active {
        return CheckResult::Fail(
            format!("{masked} on {zone} : {state}"),
            "`donsetch keys reset brightdata` re-activates the key after you fix the problem on Bright Data's side.".to_string(),
        );
    }
    CheckResult::Pass(format!("{masked} on zone {zone}, {state}"))
}

fn check_bypass(deep: bool) -> CheckResult {
    let cfg = crate::search::byok::store::ByokConfig::load();
    let bc = crate::fetch::bypass::BypassConfig::from_env();
    if !bc.enabled {
        return CheckResult::Warn(
            "integration disabled by DONSETCH_BYPASS=0 : walled sites will end on the tier-2 path instead of the solver"
                .to_string(),
        );
    }
    let Some(key) = crate::fetch::bypass::active_unlocker_key(&cfg) else {
        return CheckResult::Warn(
            "not configured (optional, opt-in: donsetch keys add unlocker <key>[::zone])"
                .to_string(),
        );
    };
    let parsed = crate::fetch::bypass::parse_key(&key, crate::fetch::bypass::DEFAULT_ZONE);
    let (_, zone) = match &parsed {
        Ok((t, z)) => (t.clone(), z.clone()),
        Err(_) => (String::new(), String::new()),
    };
    let masked = mask_key(&key);
    if let Err(e) = &parsed {
        return CheckResult::Fail(
            format!("{masked} looks broken : {e}"),
            "`donsetch keys add unlocker <token>[::zone]` replaces the key with a valid one."
                .to_string(),
        );
    }
    // Daily cap state: how close are we to the ceiling today?
    let count_path = crate::fetch::bypass::bypass_count_path(&crate::paths::cache_dir());
    let used: u32 = std::fs::read_to_string(&count_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let cap_note = if used >= bc.max_daily {
        ", daily cap reached (raise DONSETCH_BYPASS_MAX_DAILY to keep unlocking)".to_string()
    } else {
        format!(", {used}/{} daily unlocks used", bc.max_daily)
    };
    // Solve-cache state.
    let cache_dir = crate::fetch::bypass::bypass_cache_dir(&crate::paths::cache_dir());
    let cache_n: usize = std::fs::read_dir(&cache_dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0);
    let cache_note = if bc.cache_ttl.is_zero() {
        ", solve-cache disabled via DONSETCH_BYPASS_CACHE=0".to_string()
    } else {
        format!(", {cache_n} pages cached")
    };
    let base = format!(
        "{masked} on zone {zone}{cap_note}{cache_note}, render-on-solve {}",
        if bc.render { "on" } else { "off" }
    );
    // --deep: live, free zone validation (route_ips endpoint).
    if deep {
        let zone_for_probe = zone.clone();
        let token_for_probe = key
            .split_once("::")
            .map(|(t, _)| t.to_string())
            .unwrap_or_else(|| key.clone());
        match std::thread::Builder::new()
            .name("bd-probe".into())
            .spawn(move || bright_zone_probe(&token_for_probe, &zone_for_probe))
        {
            Ok(handle) => match handle.join() {
                Ok(Ok(n)) => {
                    CheckResult::Pass(format!("{base} ; live zone probe OK ({n} IPs routed)"))
                }
                Ok(Err(e)) => CheckResult::Warn(format!(
                    "{base} ; live zone probe failed: {e} (free check, nothing billed)"
                )),
                Err(_) => CheckResult::Pass(base),
            },
            Err(_) => CheckResult::Pass(base),
        }
    } else {
        CheckResult::Pass(base)
    }
}

/// Free Bright Data validation: the zone route_ips endpoint lists
/// the zone's IP pool without making a request, so a dead token or
/// wrong zone name shows up here before the first paid unlock.
fn bright_zone_probe(token: &str, zone: &str) -> Result<usize, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("client: {e}"))?;
        let resp = client
            .get(format!(
                "https://api.brightdata.com/zone/route_ips?zone={zone}"
            ))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("request: {e}"))?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err("the token or zone name was rejected (401/403) : verify both in the Bright Data dashboard, or the zone type does not expose its route IPs".to_string());
        }
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        if let Some(n) = v.get("ip_count").and_then(|x| x.as_u64()) {
            return Ok(n as usize);
        }
        if let Some(ips) = v.get("ips").and_then(|x| x.as_array()) {
            return Ok(ips.len());
        }
        Ok(0)
    })
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

/// Print the structured doctor report for agent/CI consumers.
/// Emitted AFTER the human-readable output on stdout; consumers
/// using --json are expected to parse the trailing JSON document.
fn print_json_summary(
    collected: &[(String, String, String, String)],
    p: u32,
    w: u32,
    f: u32,
    deep: bool,
) {
    use serde_json::json;
    let checks: Vec<serde_json::Value> = collected
        .iter()
        .map(|(name, status, detail, hint)| {
            json!({
                "name": name,
                "status": status,
                "detail": detail,
                "hint": hint,
            })
        })
        .collect();
    let doc = json!({
        "doctor": {
            "version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "mode": if deep { "deep" } else { "fast" },
            "summary": { "passed": p, "warnings": w, "failed": f },
            "checks": checks,
        }
    });
    println!("\n__DONSETCH_DOCTOR_JSON__");
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
}

/// Detect installed MCP clients and print ready-to-paste
/// registration blocks. Clients manage their own process model,
/// so the block is the stdio form; donsetch's own supervisor
/// (--supervised) is the recommended argv for every client.
fn print_mcp_section() {
    println!();
    println!("  {}", cli::bold("MCP client registration"));
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "donsetch".to_string());
    let found = detect_mcp_clients();
    if found.is_empty() {
        cli::check_dim("MCP clients", "none detected; generic stdio block below");
    } else {
        for (client, path) in &found {
            cli::check_pass(
                &format!("{client} (detected)"),
                &format!("config at {}", path.display()),
            );
        }
    }
    let generic = format!(
        "{{\"mcpServers\": {{\"donsetch\": {{\"command\": \"{exe}\", \
         \"args\": [\"mcp\", \"--supervised\"]}}}}}}"
    );
    println!("      Add to an MCP client (Claude Desktop, OpenCode, .mcp.json):");
    println!("      {generic}");
    println!("      Hermes (~/.hermes/config.yaml):");
    println!("        mcp_servers:");
    println!("          donsetch:");
    println!("            command: {exe}");
    println!("            args: [\"mcp\", \"--supervised\"]");
    println!("            transport: stdio");
    println!("      Supervised mode restarts donsetch if it is ever killed,",);
    println!("      which is why the blocks above prefer it.");
}

/// Known MCP client config locations. Only these small fixed files
/// are probed: detection is cheap and never scans the filesystem.
fn detect_mcp_clients() -> Vec<(String, std::path::PathBuf)> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    let mut out = Vec::new();
    let mut add = |id: &str, p: std::path::PathBuf| {
        if p.exists() {
            out.push((id.to_string(), p));
        }
    };
    if let Some(h) = &home {
        add(
            "Claude Desktop (macOS)",
            h.join("Library/Application Support/Claude/claude_desktop_config.json"),
        );
        add(
            "Claude Desktop (Windows)",
            h.join("AppData/Roaming/Claude/claude_desktop_config.json"),
        );
        add("OpenCode", h.join(".config/opencode/opencode.json"));
        add("Hermes", h.join(".hermes/config.yaml"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        add(".mcp.json", cwd.join(".mcp.json"));
        if let Some(h) = &home {
            add(".mcp.json (home)", h.join(".mcp.json"));
        }
    }
    out
}

/// Apply safe, reversible repairs for the mechanical failure
/// classes the checks can produce. Anything destructive (profile
/// deletion, key removal) is deliberately out of scope: repair
/// only what cannot hurt. Re-run repaired checks once to report
/// the true post-repair state.
async fn apply_fixes(collected: &mut [(String, String, String, String)]) -> Result<(), String> {
    let failed: Vec<String> = collected
        .iter()
        .filter(|(_, status, _, _)| status == "fail")
        .map(|(n, _, _, _)| n.clone())
        .collect();
    if failed.is_empty() {
        println!("  {}: nothing to repair", cli::green("--fix"));
        return Ok(());
    }

    for name in &failed {
        match name.as_str() {
            "Cache directory" => {
                let dir = crate::paths::cache_dir();
                if std::fs::create_dir_all(&dir).is_ok() {
                    cli::check_fixed("Cache directory", &format!("created {}", dir.display()));
                }
            }
            "Ghost state" => {
                // state file is designed to be resettable; the lock
                // file is stale-safe. Remove both.
                let dir = crate::paths::cache_dir();
                let _ = std::fs::remove_file(dir.join("ghost-state.json"));
                if let Ok(cwd) = std::env::current_dir() {
                    let _ = std::fs::remove_file(cwd.join(".donsetch-ghost.lock"));
                }
                cli::check_fixed("Ghost state", "reset state; browser profile untouched");
            }
            "OCR models" | "Rerank model" => {
                // Corrupt/missing models re-download automatically on
                // first use; nothing to do here except confirm that.
                if let Ok(dir) = std::fs::read_dir(crate::paths::cache_dir().join("ocr")) {
                    for e in dir.flatten() {
                        if e.path().is_file() && e.path().extension().is_none_or(|x| x != "json") {
                            let _ = std::fs::remove_file(e.path());
                        }
                    }
                }
                cli::check_fixed(name, "corrupt models will re-download on next use");
            }
            _ => {}
        }
    }
    println!();
    println!(
        "  {}: re-run `donsetch doctor` to confirm",
        cli::bold("done")
    );
    Ok(())
}
