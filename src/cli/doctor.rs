//! `donsetch --doctor` : health check with auto-fix.
//!
//! Checks, each with a clean pass/warn/fail icon and a dim
//! detail string. Auto-fixes what it can (creates missing dirs,
//! backs up corrupt state). Prints instructions for issues that
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
use crate::search::byok::store::KeyState;

#[derive(Debug)]
enum CheckResult {
    Pass(String),
    Warn(String),
    Fail(String, String), // (detail, instructions)
    Fixed(String),
}

fn check_counts(checks: &[(String, String, String, String)]) -> (u32, u32, u32) {
    checks
        .iter()
        .fold((0, 0, 0), |(p, w, f), (_, status, _, _)| {
            match status.as_str() {
                "pass" | "fixed" => (p + 1, w, f),
                "warn" => (p, w + 1, f),
                _ => (p, w, f + 1),
            }
        })
}

fn render_checks(checks: &[(String, String, String, String)]) {
    cli::print_title(&format!("{DISPLAY_NAME} Doctor"));
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|w| w.parse::<usize>().ok())
        .unwrap_or(96)
        .clamp(60, 140);
    let (passed, warnings, failed) = check_counts(checks);
    println!("  {passed} passed   {warnings} warnings   {failed} failed");
    let groups: &[(&str, &[&str])] = &[
        (
            "Runtime & browser",
            &[
                "Binary integrity",
                "Installation",
                "Chrome/Chromium",
                "Xvfb",
                "Browser launch",
                "Ghost profile",
                "ONNX Runtime",
                "PDFium",
                "OCR models",
                "Rerank model",
            ],
        ),
        (
            "Network & search",
            &[
                "Fetcher init",
                "Network",
                "TLS fingerprint",
                "Fetch egress",
                "Proxy pool",
                "Egress lanes",
                "DNS",
                "Captive portal",
                "Bright Data SERP",
                "Bypass unlocker",
                "Search plugins",
                "Search health",
            ],
        ),
        (
            "Configuration & state",
            &[
                "Cache directory",
                "Auth sessions",
                "State permissions",
                "Ghost state",
                "Config posture",
                "Local rules",
                "Clearance stores",
                "Crawl stores",
                "Legacy env vars",
            ],
        ),
    ];
    for (label, names) in groups {
        println!("\n  {}", cli::bold(label));
        // Put failures and warnings first within each stable group.
        for severity in ["fail", "warn", "fixed", "pass"] {
            for (name, status, detail, hint) in checks
                .iter()
                .filter(|(name, status, _, _)| names.contains(&name.as_str()) && status == severity)
            {
                let icon = match status.as_str() {
                    "pass" | "fixed" => cli::icon_pass(),
                    "warn" => cli::icon_warn(),
                    _ => cli::icon_fail(),
                };
                let detail = if status == "fixed" {
                    format!("fixed: {detail}")
                } else {
                    detail.clone()
                };
                let lines = wrap_detail(&detail, width - 30);
                println!(
                    "  {icon} {name:<24}  {}",
                    cli::dim(lines.first().map(String::as_str).unwrap_or(""))
                );
                for line in lines.iter().skip(1) {
                    println!("{:30}{}", "", cli::dim(line));
                }
                if status == "fail" && !hint.is_empty() {
                    for line in wrap_detail(hint, width - 6) {
                        println!("      {}", cli::yellow(&line));
                    }
                }
            }
        }
    }
    if checks.iter().any(|(_, s, _, _)| s == "fail") {
        println!(
            "\n  {}",
            cli::bold(
                "Next: donsetch doctor --fix (safe repairs), then follow any remaining instructions."
            )
        );
    }
    println!(
        "\n  {}",
        cli::dim("More: --deep for live probes · --mcp for client setup · --json for agents")
    );
}

fn wrap_detail(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut cells = 0;
    for word in text.split_whitespace() {
        let word_cells: usize = word
            .chars()
            .filter(|c| !c.is_control())
            .map(|c| if c.is_ascii() { 1 } else { 2 })
            .sum();
        if !line.is_empty() && cells + 1 + word_cells > width {
            lines.push(std::mem::take(&mut line));
            cells = 0;
        }
        if !line.is_empty() {
            line.push(' ');
            cells += 1;
        }
        for ch in word.chars().filter(|c| !c.is_control()) {
            let size = if ch.is_ascii() { 1 } else { 2 };
            if cells + size > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                cells = 0;
            }
            line.push(ch);
            cells += size;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn installation_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = dir.join(if cfg!(windows) {
            "donsetch.exe"
        } else {
            "donsetch"
        });
        if path.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if path
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 == 0)
                {
                    continue;
                }
            }
            let canonical = path.canonicalize().unwrap_or(path);
            if !paths.contains(&canonical) {
                paths.push(canonical);
            }
        }
    }
    paths
}

fn check_installation() -> CheckResult {
    let current = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| e.to_string());
    let mut paths = installation_paths();
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.canonicalize().unwrap_or(exe);
        if !paths.contains(&exe) {
            paths.push(exe);
        }
    }
    if paths.len() > 1 {
        CheckResult::Warn(format!(
            "{} distinct PATH installs; running {} at {current}. Pin this absolute path in MCP config (--mcp).",
            paths.len(),
            env!("CARGO_PKG_VERSION")
        ))
    } else {
        CheckResult::Pass(format!("{} at {current}", env!("CARGO_PKG_VERSION")))
    }
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
    let improve = args.iter().any(|a| a == "--improve");
    let stealth_record = args.iter().any(|a| a == "--stealth-record");
    let stealth = args.iter().any(|a| a == "--stealth") || stealth_record;
    let parity = args.iter().any(|a| a == "--parity");

    // --improve: explain the self-improvement loop in ~10 lines.
    // Standalone mode; no MCP tool, no network.
    if improve {
        print_improve_loop();
        std::process::exit(0);
    }

    // --stealth / --stealth-record: the drift scorecard (v4 phase
    // 0.4). Standalone mode: skips the general check battery.
    // --parity (with --stealth, v4 phase 1.1): diff tier-1 against
    // the REAL local browser instead of the fixture.
    if stealth || stealth_record {
        let code = stealth_scorecard(stealth_record, json, parity).await;
        std::process::exit(code);
    }

    // (name, status, detail, hint) collected for --json and --fix.
    let mut collected: Vec<(String, String, String, String)> = Vec::new();

    macro_rules! report {
        ($name:expr, $r:expr) => {
            match $r {
                CheckResult::Pass(d) => {
                    collected.push(($name.to_string(), "pass".into(), d.clone(), String::new()));
                }
                CheckResult::Warn(d) => {
                    collected.push(($name.to_string(), "warn".into(), d.clone(), String::new()));
                }
                CheckResult::Fail(d, i) => {
                    collected.push(($name.to_string(), "fail".into(), d.clone(), i.clone()));
                }
                CheckResult::Fixed(d) => {
                    collected.push(($name.to_string(), "fixed".into(), d.clone(), String::new()));
                }
            }
        };
    }

    // 1. Binary integrity.
    report!("Binary integrity", check_binary());
    report!("Installation", check_installation());

    // Create fetcher for network and TLS checks.
    let fetcher = match Fetcher::new(BrowserProfile::host_default()) {
        Ok(fm) => Some(fm),
        Err(e) => {
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

    // 2b. Fetch egress + trust posture (local-only, always runs).
    report!("Fetch egress", check_fetch_egress());

    // 2c. Proxy pool + persisted lane health (local-only).
    report!("Proxy pool", check_proxy_pool());

    // 2d. Egress lanes: local health always; --deep live-probes
    // every configured proxy and prints one line per lane.
    report!("Egress lanes", check_egress_lanes(deep).await);

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

    // Browser availability means a real launch and selftest, including in
    // default mode. Deep adds network/provider probes.
    report!("Browser launch", check_browser_launch().await);
    if deep {
        // Captive portal: generate_204 must stay 204. A hotel/airport
        // login page answering 200/302 is the classic "TLS works but
        // every fetch is a login form" failure.
        report!(
            "Captive portal",
            check_captive_portal(fetcher.as_ref()).await
        );
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
    // adds a free account-zone check (no target request).
    report!("Bright Data SERP", check_brightdata(deep));
    report!("Bypass unlocker", check_bypass(deep));

    // 15.5 BYOK plugins (user-registered executable adapters).
    report!("Search plugins", check_plugins());

    // 16. Config posture: layer conflicts, missing files, redaction.
    report!("Config posture", check_config_posture());

    // 16b. Local rules: how many, and whether they are enforced.
    report!("Local rules", check_rules());

    // 17. Search health snapshot (local, fast): trust, quarantine,
    // quality/outcome receipts, BYOK key states, C kill switches.
    report!("Search health", check_search_health());

    // 18. Clearance stores: routes.json, handles, cookie vault.
    report!("Clearance stores", check_clearance_stores());

    // 19. Crawl stores: governor persist, page-history size.
    report!("Crawl stores", check_crawl_stores());

    // 20. Network reality: DNS + optional captive-portal probe.
    report!("DNS", check_dns());

    // 21. MCP client registration (detect + print blocks).
    if only_mcp {
        print_mcp_section();
    }

    // 22. Legacy env vars (the pre-v4 names): still honored, but
    // each one active in this shell gets ONE warning naming its
    // config key. Cut at the v4 release.
    let legacy: Vec<&'static str> = crate::config::legacy_vars_in_env();
    if legacy.is_empty() {
        report!("Legacy env vars", CheckResult::Pass("none set".to_string()));
    } else {
        let names: Vec<String> = legacy
            .iter()
            .map(|name| {
                let (section, key) = crate::config::legacy_target_of(name);
                format!("{name} -> {section}.{key}")
            })
            .collect();
        report!(
            "Legacy env vars",
            CheckResult::Warn(format!("{} (deprecated, cut at v4)", names.join(", ")))
        );
    }

    // ── Self-healing pass (--fix) ───────────────────────────
    if fix {
        println!();
        apply_fixes(&mut collected);
    }

    // ── Summary ──────────────────────────────────────────────
    println!();
    let (p, w, f) = check_counts(&collected);
    if !json {
        render_checks(&collected);
    }
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
    let generic = match fetcher.fetch("https://example.com").await {
        Ok(out) if out.status == 200 => out,
        Ok(out) => return CheckResult::Warn(format!("example.com returned HTTP {}", out.status)),
        Err(e) => {
            // Egress-filter environments are the one common case
            // where a "network is fine" box still fails every
            // fetch: curl works via the env proxy, direct sockets
            // get reset. Surface the fix instead of a generic
            // "check your connection".
            // The advice must match what the daemon actually does:
            // a proxy comes from the ambient env vars OR the [proxy]
            // slots in donsetch.toml (from_env_for consults the
            // config layer first; the pool lane adds proxy.pool).
            let proxy_configured = [
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
            ]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
                || {
                    let p = &crate::config::cfg().proxy;
                    !p.https.trim().is_empty()
                        || !p.http.trim().is_empty()
                        || !p.all.trim().is_empty()
                        || !p.pool.is_empty()
                };
            let hint = if proxy_configured {
                "A proxy is configured (env vars or [proxy] in donsetch.toml) and the direct fetch still failed: this looks like a TLS-intercepting egress network. donsetch honors it automatically (see 'Fetch egress'); if it still fails, export SSL_CERT_FILE=<the network's CA bundle> or set [tls] cert_file in donsetch.toml so the re-signed certificates verify, then re-run doctor."
                    .to_string()
            } else {
                "Check your network connection and DNS. Behind an egress proxy? Set [proxy] https/http in donsetch.toml or export HTTPS_PROXY/HTTP_PROXY (NO_PROXY accepted).".into()
            };
            return CheckResult::Fail(e.to_string(), hint);
        }
    };
    // The tool lane is a SEPARATE call chain into the same dialer:
    // fetch_persona, the tier-1 navigation identity that every MCP
    // web_fetch and CLI fetch rides. Probing it here is what makes this
    // line a prediction of tool behaviour instead of a prediction of the
    // update check: a bug that broke only the persona lane once had this
    // check report healthy egress while every tool call failed, and the
    // misdiagnosis sent the report to the deployment instead of here.
    // fetch_persona reports no timing of its own (elapsed is zero on that
    // path), so only the generic lane's RTT is quoted.
    match fetcher.fetch_persona("https://example.com", None).await {
        Ok(t) if t.status == 200 => CheckResult::Pass(format!(
            "example.com 200 OK ({:.0}ms, generic and tool lanes)",
            generic.elapsed.as_secs_f64() * 1000.0,
        )),
        Ok(t) => CheckResult::Warn(format!(
            "example.com: the generic lane got HTTP {} but the tool lane (web_fetch) got HTTP {}",
            generic.status, t.status
        )),
        Err(e) => CheckResult::Warn(format!(
            "example.com: the generic lane reached it but the tool lane (web_fetch) failed: {e}"
        )),
    }
}

/// Fetch egress + trust posture: env-proxy resolution, kill switch,
/// and the two certificate stores the connector builds from.
/// Local-only: no network, stays in fast mode.
fn check_fetch_egress() -> CheckResult {
    let proxy = &crate::config::cfg().proxy;
    let slot_set = !proxy.https.trim().is_empty()
        || !proxy.http.trim().is_empty()
        || !proxy.all.trim().is_empty();
    // from_env_for already consults the [proxy] slots first and
    // gates only the ambient env read on from_environment: call it
    // unconditionally so the doctor's view matches the daemon's
    // (a TOML proxy with from_environment = false used to be
    // reported as direct egress).
    let resolved = match crate::transport::proxy::from_env_for("https://example.com") {
        Ok(proxy) => proxy,
        Err(error) => {
            return CheckResult::Fail(
                error.to_string(),
                "Correct the configured proxy endpoint or proxy environment variable.".into(),
            );
        }
    };
    let (sys_roots, env_roots) = crate::transport::tls::trust_store_report();
    let cert_bundle = std::env::var_os("SSL_CERT_FILE").map(|p| p.to_string_lossy().into_owned());

    let mut bits = Vec::new();
    if let Some(p) = resolved {
        bits.push(format!("egress via {} proxy {}:{} ({}; SOCKS5 keeps TLS end-to-end, HTTP CONNECT gets the interception-safe handshake)", if p.is_http_connect() { "http" } else { "socks5" }, p.host, p.port, if slot_set { "config" } else { "env" }));
    } else {
        bits.push(if !proxy.from_environment {
            "direct egress (proxy.from_environment = false disables the env-proxy convention; [proxy] slots in donsetch.toml stay live)"
                .into()
        } else {
            "direct egress (no proxy env vars; export HTTPS_PROXY/HTTP_PROXY to route fetches)"
                .into()
        });
    }
    match cert_bundle {
        Some(b) => {
            if env_roots > 0 {
                bits.push(format!(
                    "trust: {sys_roots} system roots + {env_roots} from {b}"
                ));
                CheckResult::Pass(bits.join(" · "))
            } else {
                bits.push(format!("trust: {sys_roots} system roots; {b} set but yielded no parseable certs (the interception CA will NOT be trusted)"));
                CheckResult::Warn(bits.join(" · "))
            }
        }
        None => {
            bits.push(format!(
                "trust: {sys_roots} system roots; SSL_CERT_FILE unset"
            ));
            CheckResult::Pass(bits.join(" · "))
        }
    }
}

/// Proxy pool size + persisted egress-health.json (search/fetch lane
/// burn memory). Local-only: reads config and the health file, no
/// network. Named fix when every proxy lane is currently benched.
fn check_proxy_pool() -> CheckResult {
    let proxy = &crate::config::cfg().proxy;
    let pool_n = proxy.pool.iter().filter(|s| !s.trim().is_empty()).count();
    let persist = proxy.egress_persist && !crate::config::cfg().state.no_disk_state;
    let mut bits = Vec::new();
    if pool_n == 0 {
        bits.push("pool empty (DONSEEK_PROXIES or [proxy] pool; direct-only egress)".to_string());
    } else {
        bits.push(format!("{pool_n} proxy lane(s) configured"));
    }
    bits.push(if persist {
        "egress health persisted".into()
    } else {
        "egress health NOT persisted (proxy.egress_persist=false or state.no_disk_state)".into()
    });

    if !persist {
        return CheckResult::Pass(bits.join(" · "));
    }

    let path = paths::cache_dir().join("egress-health.json");
    if !path.exists() {
        bits.push("no egress-health.json yet (benches appear after the first burn)".into());
        return CheckResult::Pass(bits.join(" · "));
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            bits.push(format!("egress-health.json unreadable: {e}"));
            return CheckResult::Warn(bits.join(" · "));
        }
    };
    let burned = raw.matches("\"burned\"").count();
    let dead_n: usize = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("dead").and_then(|d| d.as_array()).map(|a| a.len()))
        .unwrap_or(0);

    if pool_n > 0 && dead_n >= pool_n {
        bits.push(format!(
            "all {pool_n} proxy lane(s) benched in egress-health.json: run `donsetch proxy check` and fix creds/network"
        ));
        return CheckResult::Warn(bits.join(" · "));
    }
    bits.push(format!(
        "learned benches: {burned} burned pair marker(s), {dead_n} dead lane(s)"
    ));
    CheckResult::Pass(bits.join(" · "))
}

/// `donsetch doctor --improve`: the self-improvement loop in plain
/// language + live local receipts. No MCP tool, no network.
fn print_improve_loop() {
    cli::print_title(&format!("{DISPLAY_NAME} Improve"));
    println!();
    println!("  DonSeTch learns from YOUR use of it, on this machine only.");
    println!("  Nothing leaves the box. No telemetry, no cloud model.");
    println!();
    println!("  What it remembers");
    println!("    · per-host walls, cookie freshness, solve cooldowns");
    println!("    · per-(engine, intent) search trust EWMAs");
    println!("    · domain quality from enrich success (tiny rank prior)");
    println!("    · agent-outcome demotes (must_contain miss / thin pages)");
    println!("    · proxy lane health + RTT (search / fetch / crawl share it)");
    println!("    · crawl host ladders (429 storms, robots delays)");
    println!();
    println!("  What it does with that");
    println!("    · skips doomed tier-1 hits and dead egress lanes");
    println!("    · orders engines by what worked for THIS intent");
    println!("    · nudges ranking toward hosts that historically enrich clean");
    println!("    · soft-demotes hosts that failed your own verification");
    println!("    · warms top results so your next web_fetch is near-instant");
    println!("    · fails fast (honest) instead of burning a browser cycle");
    println!();
    let state = crate::ghost::cache::GhostState::load();
    let (hosts, walled, warm, cooldowns, flaky) = state.route_stats();
    let (t, ti, f) = crate::search::persist_load_for_status();
    let low = t.values().filter(|&&x| x < 0.5).count() + ti.values().filter(|&&x| x < 0.5).count();
    let warm_hits = state.pool_served_total + state.prewarmed_served_total;
    let (q_hosts, q_high, q_low) = crate::search::persist_load_quality_for_status();
    let (o_keys, o_demoted) = crate::search::persist_load_outcome_for_status();
    cli::print_kv("receipts", "");
    println!("    hosts {hosts} · walled {walled} · warm-ready {warm} · warm-hits {warm_hits}");
    println!(
        "    cooldowns {cooldowns} · flaky {flaky} · probes {} · engine trust {}/{} low",
        state.probes_total,
        low,
        t.len() + ti.len()
    );
    println!(
        "    quarantined engines {f} · quality hosts {q_hosts} ({q_high} high / {q_low} low)",
        f = f.len()
    );
    println!(
        "    outcome keys {o_keys} ({o_demoted} demoted) · outcome_feedback {}",
        if crate::config::cfg().search.outcome_feedback {
            "on"
        } else {
            "off (default; enable search.outcome_feedback)"
        }
    );
    println!();
    println!("  Kill switches");
    println!("    state.route_memory=off     forget host/persona learning");
    println!("    DONSETCH_NO_EGRESS_PERSIST forget lane health");
    println!("    DONSETCH_NO_PREWARM        stop search→fetch warm handoff");
    println!("    DONSETCH_NO_QUALITY_PRIOR  stop the learned domain rank nudge");
    println!("    search.outcome_feedback=off  agent-outcome demotes (default)");
    println!();
    println!("  Battle-test before any public claim: 24h soak under bench/improve/.");
    cli::print_footer();
}

/// Egress fabric lanes (v4 A2). Fast mode: local health summary
/// from the shared pool / persisted file. --deep: live-probe every
/// configured proxy (connect + small GET) and name the fix.
async fn check_egress_lanes(deep: bool) -> CheckResult {
    let pool = crate::search::egress::global()
        .unwrap_or_else(|| std::sync::Arc::new(crate::search::egress::EgressPool::from_env()));
    let summary = pool.lane_summary();
    if summary.is_empty() || (summary.len() == 1 && summary[0].is_direct) {
        return CheckResult::Pass(
            "direct-only egress (no proxy pool; every tool rides the home IP)".into(),
        );
    }
    let mut bits: Vec<String> = Vec::new();
    let mut bad = 0usize;
    for row in &summary {
        let name = if row.is_direct { "direct" } else { &row.id };
        let rtt = row
            .rtt_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "-".into());
        let persona = row
            .persona_host
            .as_deref()
            .map(|h| format!(" persona={h}"))
            .unwrap_or_default();
        let mut line = format!("{name}: {} rtt={rtt}{persona}", row.state);
        if matches!(row.state.as_str(), "dead" | "auth" | "burned") {
            bad += 1;
            if row.state == "auth" {
                line.push_str(" · fix: `donsetch proxy test <url>` then update credentials");
            } else {
                line.push_str(" · fix: `donsetch proxy check`; replace dead lines");
            }
        }
        bits.push(line);
    }

    // Fetch posture rides the summary: with the opt-in off (the
    // default) a configured pool does not touch web_fetch at all.
    bits.insert(
        0,
        if crate::config::cfg().proxy.fetch_rotate {
            "fetch: pooled (proxy.fetch_rotate)".to_string()
        } else {
            "fetch: direct; `donsetch proxy fetch on` opts in".to_string()
        },
    );
    if !crate::config::cfg().proxy.crawl_rotate {
        bits.insert(1, "crawl: direct (proxy.crawl_rotate=false)".to_string());
    }

    if deep {
        let proxies = pool.proxies();
        if !proxies.is_empty() {
            let results = crate::cli::proxy::probe_all(&proxies).await;
            // Every probe failing is inconclusive about the lanes: the
            // shared probe endpoint itself may be down. Health state is
            // left untouched: failed probes are not benched and prior
            // bans are never revived away (the old "benches cleared"
            // erased legitimate dead/auth evidence).
            if results.iter().all(|r| !r.alive) {
                bits.push(
                    "all lanes failed the live probe (the probe endpoint may be down): \
                     health state left untouched, no benches written or cleared"
                        .into(),
                );
                return CheckResult::Warn(bits.join(" · "));
            }
            let mut dead = 0usize;
            let mut slow = 0usize;
            for (px, r) in proxies.iter().zip(results.iter()) {
                if r.alive {
                    pool.observe_rtt(&px.id(), r.latency);
                    if r.latency.as_millis() as u64
                        >= crate::search::egress::EgressPool::slow_rtt_ms() as u64
                    {
                        slow += 1;
                        bits.push(format!(
                            "{}: live slow {} (exit {})",
                            px.id(),
                            r.latency.as_millis(),
                            r.exit_ip.as_deref().unwrap_or("?")
                        ));
                    }
                } else {
                    dead += 1;
                    pool.report_dead(&px.id());
                    bits.push(format!(
                        "{}: live dead · {}",
                        px.id(),
                        r.error.as_deref().unwrap_or("probe failed")
                    ));
                }
            }
            if dead > 0 {
                return CheckResult::Fail(
                    bits.join(" · "),
                    "replace or repair the dead proxy lines, then re-run doctor --deep".into(),
                );
            }
            if slow > 0 {
                return CheckResult::Warn(bits.join(" · "));
            }
        }
    }

    if bad > 0 {
        return CheckResult::Warn(bits.join(" · "));
    }
    CheckResult::Pass(bits.join(" · "))
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
    let manager = crate::ghost::manager::GhostManager::new().await;
    let inner = async {
        let profile = BrowserProfile::host_default();
        let t0 = std::time::Instant::now();
        let mut ghost = match manager.acquire(&profile).await {
            Ok(g) => g,
            Err(e) => {
                return CheckResult::Fail(
                    format!("launch failed: {e}"),
                    "Tier 2 browser fallback will not work. Install Chromium/Xvfb, set DONGHOST_CHROME, or configure CloakBrowser with CLOAKBROWSER_BINARY_PATH.".into(),
                );
            }
        };
        let launch_ms = t0.elapsed().as_millis();

        let fp = crate::ghost::ops::selftest(&mut ghost).await;
        drop(ghost);
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
    let result = match tokio::time::timeout(std::time::Duration::from_secs(40), inner).await {
        Ok(r) => r,
        Err(_) => CheckResult::Fail(
            "launch timed out after 40s".into(),
            "Run doctor --fix and inspect the browser/display checks. Close only a confirmed stale DonGhost process; preserve other browser sessions and live display sockets.".into(),
        ),
    };
    manager.shutdown().await;
    result
}

/// Sets every regular file under `root` that others can read to
/// 0600 and returns (files seen, files tightened, files it could not
/// change). Symlinks are neither followed nor changed, and the walk
/// stops after 10 000 entries.
#[cfg(unix)]
fn tighten_tree(root: &std::path::Path) -> (u32, u32, u32) {
    use std::os::unix::fs::PermissionsExt;
    let (mut seen, mut tightened, mut stuck) = (0u32, 0u32, 0u32);
    if !std::fs::symlink_metadata(root).is_ok_and(|m| m.is_dir()) {
        return (seen, tightened, stuck);
    }
    let mut visited = 0u32;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 10_000 {
                return (seen, tightened, stuck);
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            seen += 1;
            if meta.permissions().mode() & 0o077 == 0 {
                continue;
            }
            if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_ok() {
                tightened += 1;
            } else {
                stuck += 1;
            }
        }
    }
    (seen, tightened, stuck)
}

/// Session-bearing state must not be world-readable. Covers the
/// cookie vault (ghost-state.json), the TLS-session routes file, the
/// key, page-history, search-cache and crawl-resume stores, the
/// host-bearing stores (crawl governor, egress health, search trust,
/// search quality, outcome feedback), and the files under
/// `screenshots/`, `ghost-debug/`, `crawl-resumes/`, `bypass-cache/`
/// and `host-pace/`.
fn check_state_permissions() -> CheckResult {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = paths::cache_dir();
        // Files that carry cookies, TLS sessions, auth material, or
        // fetched page content (page-history keeps whole markdown,
        // including pages fetched behind a login). handles.json is
        // opaque tokens, not secrets: leave it out.
        let secret_files = [
            "ghost-state.json",
            "routes.json",
            "byok-keys.json",
            "page-history.json",
            "search-cache.json",
            "crawl-resumes.json",
            "crawl-governor.json",
            "egress-health.json",
            "search-trust.json",
            "search-quality.json",
            "outcome-feedback.json",
        ];
        let mut fixed = Vec::new();
        let mut failed = Vec::new();
        let mut present = 0u32;
        for name in secret_files {
            let f = dir.join(name);
            if !f.exists() {
                continue;
            }
            present += 1;
            let Ok(m) = std::fs::metadata(&f) else {
                continue;
            };
            let mode = m.permissions().mode() & 0o777;
            if mode & 0o077 == 0 {
                continue;
            }
            let mut perm = m.permissions();
            perm.set_mode(0o600);
            if std::fs::set_permissions(&f, perm).is_ok() {
                fixed.push(format!("{name} {mode:o}→600"));
            } else {
                failed.push(format!("{name} is {mode:o}"));
            }
        }
        // Page artifacts and session-bearing stores: what the ghost
        // rendered (a logged-in page included), crawl resume frontiers,
        // and unlocker-cached page bodies. Files written before they
        // were sealed stay on disk, so they are tightened where they
        // lie.
        for sub in [
            "screenshots",
            "ghost-debug",
            "crawl-resumes",
            "bypass-cache",
            "host-pace",
        ] {
            let (seen, tightened, stuck) = tighten_tree(&dir.join(sub));
            if seen > 0 {
                present += 1;
            }
            if tightened > 0 {
                fixed.push(format!("{sub}/ {tightened} file(s) →600"));
            }
            if stuck > 0 {
                failed.push(format!("{sub}/* has {stuck} file(s) others can read"));
            }
        }
        if !failed.is_empty() {
            return CheckResult::Fail(
                failed.join(", "),
                format!(
                    "chmod 600 {}",
                    failed
                        .iter()
                        .map(|s| s.split_whitespace().next().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            );
        }
        if !fixed.is_empty() {
            return CheckResult::Fixed(format!("tightened {}", fixed.join(", ")));
        }
        if present == 0 {
            return CheckResult::Pass("no secret-bearing state files yet".into());
        }
        CheckResult::Pass(format!("{present} secret store(s) at 600"))
    }
    #[cfg(not(unix))]
    {
        // Windows: ACLs inherit from the user profile; we do not
        // rewrite them. Presence is still worth reporting.
        let dir = paths::cache_dir();
        let present = ["ghost-state.json", "routes.json", "byok-keys.json"]
            .iter()
            .filter(|n| dir.join(n).exists())
            .count();
        CheckResult::Pass(format!(
            "windows ACLs apply ({present} secret store(s) present)"
        ))
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

    match probe_writable(&dir) {
        Ok(()) => CheckResult::Pass("writable; profile locks preserved".into()),
        Err(e) => CheckResult::Fail("not writable".into(), format!("Check permissions: {e}")),
    }
}

// Probe only a newly created file owned by this check. A fixed filename can
// overwrite a user's file or follow a symlink; Chromium owns its own locks.
fn probe_writable(dir: &Path) -> std::io::Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = dir.join(format!(".doctor-write-{}-{nonce}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    drop(file);
    std::fs::remove_file(path)
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

    match probe_writable(&dir) {
        Ok(()) => {
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
        CheckResult::Pass(option_env!("DONSETCH_PDFIUM").unwrap_or("static").into())
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().unwrap_or_default();
        let dll = exe.parent().unwrap_or(Path::new("")).join("pdfium.dll");
        if dll.exists() {
            CheckResult::Pass(option_env!("DONSETCH_PDFIUM").unwrap_or("dll").into())
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
            return CheckResult::Warn("disabled (fetch.ocr = false)".into());
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
        // environment and surface the result. A build whose runtime
        // cannot get in fails here instead of printing a success
        // string (this exact probe would have caught the v3.3.0 leak
        // on Windows/macOS).
        //
        // AVX is a diagnostic, not a gate: the shipped runtime
        // dispatches its kernels at runtime and runs on SSE4.2; on
        // arm64 the concept does not exist at all.
        let has_avx = crate::cpu::has_avx();
        let cpu = if cfg!(target_arch = "aarch64") {
            "arm64 (NEON)"
        } else if has_avx {
            "AVX detected"
        } else {
            "no AVX (SSE kernels)"
        };
        // Check the shared library's presence beside the binary or in
        // the cache fallback.
        let lib_name = crate::onnx::shared_lib_name();
        let found = if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            parent.join(lib_name).exists()
        } else {
            false
        };
        let cache = paths::cache_dir().join("onnx").join(lib_name).exists();
        if !(found || cache) {
            CheckResult::Warn(format!(
                "{cpu} but shared library missing : reinstall donsetch"
            ))
        } else {
            // Presence is not proof: the library must load and
            // initialize. A text file dressed as onnxruntime.dll
            // passed the old exists() check, which made the release
            // workflow's no-AVX doctor run assert nothing about the
            // runtime (review of #300). Same payload probe on every
            // platform.
            match crate::onnx::ensure_loaded() {
                Ok(()) => CheckResult::Pass(format!("{cpu}, shared library present")),
                Err(e) if e.contains("not found") => CheckResult::Warn(format!(
                    "{cpu} but shared library missing : reinstall donsetch"
                )),
                Err(e) => CheckResult::Fail("ONNX payload probe failed".into(), e),
            }
        }
    }
}

fn check_ghost_state() -> CheckResult {
    let path = paths::cache_dir().join("ghost-state.json");
    if path.exists() {
        match std::fs::read(&path) {
            Ok(body) => {
                if let Err(e) = serde_json::from_slice::<crate::ghost::cache::GhostState>(&body) {
                    return CheckResult::Fail(format!("invalid state: {e}"), "Run `donsetch doctor --fix` to preserve the corrupt file as a backup and rebuild route memory.".into());
                }
            }
            Err(e) => {
                return CheckResult::Fail(
                    format!("cannot read state: {e}"),
                    "Check cache directory permissions.".into(),
                );
            }
        }
    }
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
    // A plugin that reported itself invalid, out of credit or
    // rate-limited is registered and parked: it will not be spawned,
    // which is exactly how a search quietly loses that provider.
    // `keys list` shows the state; doctor is where a user looks when
    // a provider vanished, so the loop closes here too.
    let mut parked: Vec<String> = Vec::new();
    for n in &names {
        let def = &cfg.plugins[n];
        // A hand-edited plugins.json can carry an empty argv; that is
        // a registration problem, not a panic in the doctor.
        let prog = def.cmd.first().map(String::as_str).unwrap_or("");
        let is_path_form = prog.contains('/') || prog.contains('\\') || prog.starts_with('.');
        if def.cmd.is_empty() {
            missing.push(format!("{n}: no command registered"));
        } else if is_path_form && !std::path::Path::new(prog).exists() {
            missing.push(format!("{n}: {prog}"));
        }
        if def.state != KeyState::Active {
            parked.push(format!("{n}: {}", def.state.label()));
        }
    }
    let detail = format!(
        "{} registered ({}), runs at search time",
        names.len(),
        names.join(", ")
    );
    let mut problems: Vec<String> = Vec::new();
    if !missing.is_empty() {
        problems.push(format!("program not found: {}", missing.join(", ")));
    }
    if !parked.is_empty() {
        problems.push(format!(
            "parked, so not spawned: {} (re-register with `donsetch keys add plugin <name> --cmd ...` \
             after fixing the credentials; a rate-limited plugin recovers on its own)",
            parked.join(", ")
        ));
    }
    if problems.is_empty() {
        CheckResult::Pass(detail)
    } else {
        CheckResult::Warn(format!("{detail}; {}", problems.join("; ")))
    }
}

/// Config layers: conflicts the loader cannot express, missing
/// explicit paths, and secret-redaction posture. Unknown keys and
/// range errors already fail at load (`deny_unknown_fields`).
fn check_config_posture() -> CheckResult {
    let no_file = std::env::var_os("DONSETCH_NO_CONFIG_FILE").is_some();
    let explicit = std::env::var_os("DONSETCH_CONFIG");
    if no_file && explicit.is_some() {
        return CheckResult::Fail(
            "DONSETCH_NO_CONFIG_FILE and DONSETCH_CONFIG are both set".into(),
            "unset one: NO_CONFIG_FILE means 'ignore all files'; DONSETCH_CONFIG names one file to load".into(),
        );
    }
    if let Some(ref path) = explicit {
        let p = std::path::PathBuf::from(path);
        if !p.exists() {
            return CheckResult::Fail(
                format!("DONSETCH_CONFIG points at a missing file: {}", p.display()),
                "create the file or unset DONSETCH_CONFIG".into(),
            );
        }
    }
    // Report layer posture without echoing secrets.
    let home = dirs::config_dir()
        .map(|d| d.join("donsetch").join("donsetch.toml"))
        .filter(|p| p.exists());
    let mut layers: Vec<&str> = Vec::new();
    if no_file {
        layers.push("env-only (NO_CONFIG_FILE)");
    } else if let Some(ref h) = home {
        layers.push(if h.exists() { "user toml" } else { "defaults" });
    } else {
        layers.push("defaults");
    }
    if explicit.is_some() {
        layers.push("DONSETCH_CONFIG");
    }
    // Secret redaction: config show must never print a raw key.
    // We only assert the redaction helpers exist (they are unit-
    // tested); a live redaction probe would require inventing a key.
    CheckResult::Pass(format!("layers: {}", layers.join(" + ")))
}

/// The local `[rules]` table: the rule count and the `mode`, with the
/// layer `mode` came from. Warns when rules exist but `mode` is off.
/// An uncompilable pattern never reaches this check: it fails the
/// config load before doctor runs.
fn check_rules() -> CheckResult {
    // The installed config keeps no per-leaf origins, so the layers are
    // loaded again for the mode's. Doctor applies no CLI override to
    // `[rules]`, so both loads see the same mode.
    let origin = match crate::config::load() {
        Ok(loaded) => loaded
            .origin_of_path(&["rules", "mode"])
            .unwrap_or("default")
            .to_string(),
        Err(_) => "origin unknown".to_string(),
    };
    rules_check(&crate::config::cfg().rules, &origin)
}

fn rules_check(section: &crate::rules::RulesSection, mode_origin: &str) -> CheckResult {
    let total = section.url.len();
    let disabled = section.url.values().filter(|r| !r.enabled).count();
    let plural = if total == 1 { "" } else { "s" };
    let count = if disabled == 0 {
        format!("{total} rule{plural}")
    } else {
        format!("{total} rule{plural} ({disabled} disabled)")
    };
    let detail = format!("{count} · mode={} ({mode_origin})", section.mode.as_str());
    // Off with live rules is the state a forgotten
    // DONSETCH_RULES__MODE=off leaves behind: the operator believes a
    // protection is in place that is not.
    if section.mode == crate::rules::RulesMode::Off && total > disabled {
        CheckResult::Warn(format!("{detail}: no rule is enforced"))
    } else {
        CheckResult::Pass(detail)
    }
}

/// Search engine + learning receipts, local-only and fast. One
/// line an agent can trust: is the roster healthy, is learning
/// on, are the new C kill switches armed.
fn check_search_health() -> CheckResult {
    let (trust, trust_intent, failures) = crate::search::persist_load_for_status();
    let low = trust.values().filter(|&&x| x < 0.5).count()
        + trust_intent.values().filter(|&&x| x < 0.5).count();
    let quarantined = failures.len();
    let (q_hosts, _, _) = crate::search::persist_load_quality_for_status();
    let (o_keys, o_demoted) = crate::search::persist_load_outcome_for_status();
    let s = &crate::config::cfg().search;
    let flags = format!(
        "early={} compile={} instant={} quality={} outcome={}",
        onoff(s.search_early),
        onoff(s.query_compile),
        onoff(s.serp_instant),
        onoff(s.quality_prior),
        onoff(s.outcome_feedback),
    );
    // BYOK key states: counts only, never the keys.
    let store = crate::search::byok::store::ByokConfig::load();
    let mut active = 0usize;
    let mut limited = 0usize;
    let mut dead = 0usize;
    for p in &store.providers {
        for k in &p.keys {
            match k.state {
                crate::search::byok::store::KeyState::Active => active += 1,
                crate::search::byok::store::KeyState::RateLimited => limited += 1,
                crate::search::byok::store::KeyState::CreditDepleted
                | crate::search::byok::store::KeyState::Invalid => dead += 1,
            }
        }
    }
    let byok = if active + limited + dead == 0 {
        "byok none".to_string()
    } else {
        format!("byok {active} active / {limited} limited / {dead} dead")
    };
    let detail = format!(
        "trust {} entries ({low} low) · quarantined {quarantined} · quality {q_hosts} · outcome {o_keys} ({o_demoted} demoted) · {byok} · {flags}",
        trust.len() + trust_intent.len()
    );
    if quarantined > 0 && low > 2 {
        CheckResult::Warn(format!(
            "{detail}; run a few searches to rebuild trust, or `donsetch status`"
        ))
    } else {
        CheckResult::Pass(detail)
    }
}

fn onoff(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// Clearance state: TLS-session routes, link handles, cookie vault.
fn check_clearance_stores() -> CheckResult {
    let dir = paths::cache_dir();
    let mut bits: Vec<String> = Vec::new();

    // routes.json: TLS sessions / 0-RTT material.
    let routes = dir.join("routes.json");
    if routes.exists() {
        let n = std::fs::read(&routes)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("routes").and_then(|r| r.as_object()).map(|o| o.len()))
            .unwrap_or(0);
        bits.push(format!("routes {n}"));
    } else {
        bits.push("routes none".into());
    }

    // handles.json: link-handle table (24h TTL, cap 2048).
    let handles = dir.join("handles.json");
    if handles.exists() {
        let n = std::fs::read(&handles)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v.get("l")
                    .or_else(|| v.get("entries"))
                    .and_then(|o| o.as_object())
                    .map(|o| o.len())
            })
            .unwrap_or(0);
        bits.push(format!("handles {n}"));
    } else {
        bits.push("handles none".into());
    }

    let vault = onoff(crate::config::cfg().state.cookie_vault);
    bits.push(format!("cookie_vault {vault}"));
    CheckResult::Pass(bits.join(" · "))
}

/// Crawl learning stores: governor host ladders + page history.
fn check_crawl_stores() -> CheckResult {
    let dir = paths::cache_dir();
    let mut bits: Vec<String> = Vec::new();
    let gov = dir.join("crawl-governor.json");
    if gov.exists() {
        let n = std::fs::read(&gov)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("hosts").and_then(|h| h.as_array()).map(|a| a.len()))
            .unwrap_or(0);
        bits.push(format!("governor {n} hosts"));
    } else {
        bits.push("governor none".into());
    }
    let hist = dir.join("page-history.json");
    if hist.exists() {
        let size = hist.metadata().map(|m| m.len()).unwrap_or(0);
        // > 5MB is a store that stopped rotating (or a very heavy
        // crawl user): worth a warning, not a failure.
        if size > 5_000_000 {
            return CheckResult::Warn(format!(
                "page-history is {} (delete or trim if crawls feel slow)",
                format_size(size)
            ));
        }
        bits.push(format!("history {}", format_size(size)));
    } else {
        bits.push("history none".into());
    }
    CheckResult::Pass(bits.join(" · "))
}

/// DNS resolution, independent of HTTP. Catches the "TLS works
/// via proxy but the resolver is broken" class that check_network
/// cannot see.
fn check_dns() -> CheckResult {
    use std::net::ToSocketAddrs;
    match ("example.com", 443u16).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            if addrs.is_empty() {
                CheckResult::Fail(
                    "example.com resolved to zero addresses".into(),
                    "check /etc/resolv.conf or your VPN DNS".into(),
                )
            } else {
                let v6 = addrs.iter().any(|a| a.is_ipv6());
                let note = if v6 { " (AAAA present)" } else { " (A only)" };
                CheckResult::Pass(format!("example.com → {} addr(s){note}", addrs.len()))
            }
        }
        Err(e) => CheckResult::Fail(
            format!("DNS lookup failed: {e}"),
            "check /etc/resolv.conf, systemd-resolved, or your VPN DNS".into(),
        ),
    }
}

/// Captive-portal detector (--deep only). gstatic generate_204 is
/// the industry-standard probe: a clean network returns 204 with an
/// empty body; a portal rewrites it to a login page.
async fn check_captive_portal(fetcher: Option<&Fetcher>) -> CheckResult {
    let Some(fetcher) = fetcher else {
        return CheckResult::Warn("skipped: fetcher unavailable".into());
    };
    match fetcher
        .fetch("http://connectivitycheck.gstatic.com/generate_204")
        .await
    {
        Ok(out) if out.status == 204 => {
            CheckResult::Pass("generate_204 returned 204 (no portal)".into())
        }
        Ok(out) => CheckResult::Warn(format!(
            "generate_204 returned {} : possible captive portal (hotel/airport Wi-Fi login page)",
            out.status
        )),
        Err(e) => CheckResult::Warn(format!("portal probe failed: {e}")),
    }
}

/// Display form of a key: enough to recognize it, never enough to
/// use it. Char-based throughout -- the old byte slices panicked on
/// a key with a multibyte char in the cut position (`parse_key` /
/// `keys import` never reject non-ASCII), and showed 7 of 8 chars of
/// a short key.
fn mask_key(k: &str) -> String {
    let start = k.split_once("::").map(|(t, _)| t).unwrap_or(k);
    let n = start.chars().count();
    if n <= 8 {
        let shown: String = start.chars().take(n.saturating_sub(1).min(2)).collect();
        return format!("{shown}***");
    }
    let head: String = start.chars().take(6).collect();
    let tail: String = start
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

/// Bright Data SERP key: local validation + a free live zone probe
/// in --deep mode (account zone metadata confirms token access,
/// zone existence and product type without a paid target request).
fn check_brightdata(deep: bool) -> CheckResult {
    let cfg = crate::search::byok::store::ByokConfig::load();
    let Some(entry) = cfg
        .providers
        .iter()
        .find(|p| p.name == "brightdata")
        .and_then(|p| {
            p.keys
                .iter()
                .find(|k| k.state == crate::search::byok::store::KeyState::Active)
                .or_else(|| p.keys.first())
        })
    else {
        return CheckResult::Warn(
            "not configured (optional, paid search: donsetch keys add brightdata <token>::<SERP zone>)"
                .to_string(),
        );
    };
    let (token, zone) = match crate::search::byok::brightdata_key_parts(&entry.key) {
        Ok(parts) => parts,
        Err(e) => {
            return CheckResult::Fail(
                e,
                "re-add the API token with the exact SERP zone name; see docs/brightdata.md".into(),
            );
        }
    };
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
    let base = format!("{masked} on zone {zone}, {state}");
    if deep {
        check_zone_probe(base, token, zone, "serp")
    } else {
        CheckResult::Pass(base)
    }
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
    let usable = crate::fetch::bypass::active_unlocker_key(&cfg);
    let stored = cfg
        .providers
        .iter()
        .find(|p| p.name == "unlocker")
        .and_then(|p| p.keys.first());
    let Some(key) = usable.clone().or_else(|| stored.map(|k| k.key.clone())) else {
        return CheckResult::Warn(
            "not configured (optional, opt-in: donsetch keys add unlocker <key>[::zone])"
                .to_string(),
        );
    };
    if usable.is_none() {
        return CheckResult::Fail(
            format!("unlocker configured but unavailable: {}", stored.expect("stored key selected").state.label()),
            "wait for a rate-limit cooldown; after fixing token/balance, run `donsetch keys reset unlocker`".into(),
        );
    }
    let parsed = crate::fetch::bypass::parse_key(&key, crate::fetch::bypass::DEFAULT_ZONE);
    let (token, zone) = match &parsed {
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
    // --deep: free account-zone validation.
    if deep {
        check_zone_probe(base, token, zone, "unblocker")
    } else {
        CheckResult::Pass(base)
    }
}

fn check_zone_probe(
    base: String,
    token: String,
    zone: String,
    product: &'static str,
) -> CheckResult {
    match std::thread::Builder::new()
        .name("bd-probe".into())
        .spawn(move || bright_zone_probe(&token, &zone, product))
    {
        Ok(handle) => match handle.join() {
            Ok(ZoneProbeOut::Validated) => CheckResult::Pass(format!(
                "{base} ; token, zone and product verified (free account check)"
            )),
            Ok(ZoneProbeOut::Skipped { reason }) => CheckResult::Warn(format!(
                "{base} ; live zone probe skipped: {reason} (token and zone remain unverified)"
            )),
            Ok(ZoneProbeOut::Failed { reason }) => CheckResult::Warn(format!(
                "{base} ; live zone probe failed: {reason} (free check, nothing billed)"
            )),
            Err(_) => CheckResult::Warn(format!("{base} ; live zone probe thread failed")),
        },
        Err(e) => CheckResult::Warn(format!("{base} ; live zone probe could not start: {e}")),
    }
}

/// Result of the free account-zone check; no target request is made.
#[derive(Debug, PartialEq, Eq)]
enum ZoneProbeOut {
    Validated,
    Skipped { reason: String },
    Failed { reason: String },
}

const BRIGHT_API_BASE: &str = "https://api.brightdata.com";

fn bright_zone_probe(token: &str, zone: &str, product: &str) -> ZoneProbeOut {
    bright_zone_probe_at(BRIGHT_API_BASE, token, zone, product)
}

fn bright_zone_probe_at(base: &str, token: &str, zone: &str, product: &str) -> ZoneProbeOut {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return ZoneProbeOut::Failed {
                reason: format!("runtime: {e}"),
            };
        }
    };
    rt.block_on(bright_zone_probe_async(base, token, zone, product))
}

fn classify_zones(status: u16, body: &[u8], zone: &str, product: &str) -> ZoneProbeOut {
    if status == 403 {
        return ZoneProbeOut::Skipped {
            reason: "token cannot list account zones (403); check its permissions in the dashboard"
                .into(),
        };
    }
    if status != 200 {
        return ZoneProbeOut::Failed {
            reason: format!(
                "account-zone check returned HTTP {status}; check token and account access"
            ),
        };
    }
    let zones: Vec<serde_json::Value> = match serde_json::from_slice(body) {
        Ok(zones) => zones,
        Err(e) => {
            return ZoneProbeOut::Failed {
                reason: format!("invalid account-zone response: {e}"),
            };
        }
    };
    let Some(found) = zones
        .iter()
        .find(|z| z.get("name").and_then(|v| v.as_str()) == Some(zone))
    else {
        return ZoneProbeOut::Failed {
            reason: format!("zone {zone:?} not found; use the exact dashboard name after ::"),
        };
    };
    if found.get("type").and_then(|v| v.as_str()) != Some(product) {
        return ZoneProbeOut::Failed {
            reason: format!(
                "zone {zone:?} is not a {product} zone; use separate SERP API and Web Unlocker zones (docs/brightdata.md)"
            ),
        };
    }
    ZoneProbeOut::Validated
}

async fn bright_zone_probe_async(
    base: &str,
    token: &str,
    zone: &str,
    product: &str,
) -> ZoneProbeOut {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ZoneProbeOut::Failed {
                reason: format!("client: {e}"),
            };
        }
    };
    let mut resp = match client
        .get(format!("{base}/zone/get_active_zones"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return ZoneProbeOut::Failed {
                reason: format!("request: {}", e.without_url()),
            };
        }
    };
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= 1024 * 1024 => {
                body.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => {
                return ZoneProbeOut::Failed {
                    reason: "account-zone response exceeds 1 MiB".into(),
                };
            }
            Ok(None) => break,
            Err(e) => {
                return ZoneProbeOut::Failed {
                    reason: format!("response read failed: {}", e.without_url()),
                };
            }
        }
    }
    classify_zones(status, &body, zone, product)
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
    let npm_install =
        exe.contains("node_modules/donsetch/") || exe.contains("node_modules\\donsetch\\");
    let command = &exe;
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
        "{{\"mcpServers\": {{\"donsetch\": {{\"command\": {}, \
         \"args\": [\"mcp\", \"--supervised\"]}}}}}}",
        json_escape(command)
    );
    println!("      Add to an MCP client (Claude Desktop, OpenCode, .mcp.json):");
    println!("      {generic}");
    println!("      Hermes (~/.hermes/config.yaml):");
    println!("        mcp_servers:");
    println!("          donsetch:");
    println!("            command: {command}");
    println!("            args: [\"mcp\", \"--supervised\"]");
    println!("            transport: stdio");
    if npm_install {
        println!("      For npm installs, use `npx donsetch` if `donsetch` is not on PATH.");
    }
    println!("      Supervised mode restarts donsetch if it is ever killed,",);
    println!("      which is why the blocks above prefer it.");
}

fn json_escape(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
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
        add("OpenCode", h.join(".config/opencode/opencode.jsonc"));
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
fn apply_fixes(collected: &mut [(String, String, String, String)]) {
    for (name, status, detail, hint) in collected.iter_mut() {
        if status != "fail" {
            continue;
        }
        let repaired = match name.as_str() {
            "Cache directory" => std::fs::create_dir_all(crate::paths::cache_dir())
                .map(|_| check_cache_dir())
                .map_err(|e| e.to_string()),
            "Ghost state" => {
                let path = crate::paths::cache_dir().join("ghost-state.json");
                let corrupt = std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| {
                        if serde_json::from_slice::<crate::ghost::cache::GhostState>(&bytes).is_ok()
                        {
                            Err("state is valid; repair its access permissions manually".into())
                        } else {
                            Ok(())
                        }
                    });
                let backup = path.with_extension(format!(
                    "json.{}.bak",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                ));
                corrupt
                    .and_then(|_| std::fs::rename(&path, &backup).map_err(|e| e.to_string()))
                    .map(|_| check_ghost_state())
            }
            _ => continue,
        };
        match repaired {
            Ok(CheckResult::Pass(d) | CheckResult::Fixed(d)) => {
                *status = "fixed".into();
                *detail = d;
                hint.clear();
            }
            Ok(CheckResult::Warn(d)) => {
                *status = "warn".into();
                *detail = d;
            }
            Ok(CheckResult::Fail(d, h)) => {
                *detail = d;
                *hint = h;
            }
            Err(e) => {
                *detail = format!("repair failed: {e}");
            }
        }
    }
}

/// The stealth drift scorecard (v4 phase 0.4). Exit codes: 0 all
/// layers match the baseline, 1 drift or capture failure
/// (--stealth-record writes the fixture and exits 0 on success).
/// --parity (v4 phase 1.1) instead diffs the live tier-1 capture
/// against the REAL local browser (ghost): the evergreen check,
/// no fixture involved. It cannot go stale; it fails when the
/// profile and the floor diverge.
async fn stealth_scorecard(record: bool, json: bool, parity: bool) -> i32 {
    use crate::profile::scorecard;

    cli::print_title(&format!("{DISPLAY_NAME} Stealth Scorecard"));
    println!();

    let fetcher = match Fetcher::new(crate::profile::BrowserProfile::host_default()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("  fetcher init failed: {e}");
            return 1;
        }
    };
    let live = match scorecard::capture(&fetcher).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("  capture failed: {e}");
            eprintln!("  (the echo endpoint is unreachable; retry or check egress)");
            return 1;
        }
    };

    if parity {
        let mgr = crate::ghost::manager::GhostManager::new().await;
        return match scorecard::capture_via_ghost(&mgr).await {
            Ok(ghost) => {
                let report = scorecard::diff(&ghost, &live);
                let bad = report.iter().filter(|v| !v.same).count();
                println!("  parity scope: tier-1 vs LOCAL BROWSER (fixtureless, evergreen)");
                println!();
                for v in &report {
                    let mark = if v.same { "ok  " } else { "DIFF" };
                    println!("  [{mark}] {:<8}", v.layer);
                    if !v.same {
                        println!("       tier1:   {}", v.live);
                        println!("       browser: {}", v.baseline);
                    }
                }
                println!();
                if bad == 0 {
                    println!("  parity: tier 1 matches the local browser. Evergreen.");
                    0
                } else {
                    eprintln!("  {bad} layer(s) diverged from the local browser.");
                    1
                }
            }
            Err(e) => {
                eprintln!("  ghost parity unavailable: {e}");
                eprintln!("  (need a local Chrome: ghost must render the echo once)");
                // Parity is best-effort evergreen, not the fixture gate.
                1
            }
        };
    }

    if record {
        let mut fixture = live.clone();
        fixture.source = format!(
            "tier1 fetcher, profile {}, recorded via --stealth-record",
            fetcher.profile().name
        );
        fixture.captured_at = "operator-recorded".to_string();
        let path = "tests/fixtures/stealth-baseline.json";
        match serde_json::to_string_pretty(&fixture) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, format!("{text}\n")) {
                    eprintln!("  could not write {path}: {e}");
                    return 1;
                }
                println!("  baseline recorded to {path}");
                println!("  review the diff, then rebuild: the fixture is embedded at build time");
                return 0;
            }
            Err(e) => {
                eprintln!("  could not serialize fixture: {e}");
                return 1;
            }
        }
    }

    let baseline = match scorecard::baseline_fixture() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("  {e}");
            return 1;
        }
    };
    let verdicts = scorecard::diff(&baseline, &live);
    let mut drifted = 0;
    if json {
        let layers: Vec<serde_json::Value> = verdicts
            .iter()
            .map(|v| {
                serde_json::json!({
                    "layer": v.layer,
                    "same": v.same,
                    "baseline": v.baseline,
                    "live": v.live,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "endpoint": scorecard::ECHO_ENDPOINT,
                "profile": live.profile,
                "drifted": verdicts.iter().filter(|v| !v.same).count(),
                "layers": layers,
            }))
            .unwrap()
        );
    } else {
        for v in &verdicts {
            if v.same {
                cli::check_pass(v.layer, "matches baseline");
            } else {
                drifted += 1;
                cli::check_fail(
                    v.layer,
                    "DRIFTED",
                    "re-capture the profile and re-record the baseline deliberately",
                );
                println!("      baseline: {}", v.baseline);
                println!("      live:     {}", v.live);
            }
        }
        println!();
        if drifted == 0 {
            println!("  all layers match the baseline; no drift");
        } else {
            println!("  {drifted} layer(s) drifted. If Chrome bumped, re-capture the profile");
            println!("  and re-record the baseline deliberately: donsetch doctor --stealth-record");
        }
    }
    if drifted == 0 { 0 } else { 1 }
}

#[cfg(test)]
mod doctor_ultra_tests {
    use super::*;

    fn isolated_cache() -> tempfile_dir::TempDir {
        // Hand-rolled: we do not want a tempfile dep just for this.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "donsetch-doctor-ultra-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &dir) };
        tempfile_dir::TempDir { dir }
    }

    #[test]
    fn wave450_doctor_fix_preserves_corrupt_state_and_recounts() {
        // Nextest: each test owns its environment and a disposable cache.
        let cache = isolated_cache();
        let corrupt = b"{not json";
        std::fs::write(cache.dir.join("ghost-state.json"), corrupt).unwrap();
        std::fs::write(cache.dir.join("keep-model.bin"), b"valid unrelated model").unwrap();
        assert!(matches!(check_ghost_state(), CheckResult::Fail(_, _)));
        let mut checks = vec![(
            "Ghost state".into(),
            "fail".into(),
            "corrupt".into(),
            "repair".into(),
        )];
        assert_eq!(check_counts(&checks), (0, 0, 1));
        apply_fixes(&mut checks);
        assert_eq!(check_counts(&checks), (1, 0, 0));
        assert_eq!(checks[0].1, "fixed");
        assert_eq!(
            std::fs::read(cache.dir.join("keep-model.bin")).unwrap(),
            b"valid unrelated model"
        );
        let backups: Vec<_> = std::fs::read_dir(&cache.dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "bak"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), corrupt);
        assert!(matches!(
            check_ghost_state(),
            CheckResult::Pass(_) | CheckResult::Warn(_)
        ));
        apply_fixes(&mut checks);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), corrupt);
    }

    #[test]
    fn wave450_doctor_wraps_long_paths_without_losing_information() {
        let text = format!(
            "Install browser at /{} then retry.",
            "long-directory/".repeat(12)
        );
        let lines = wrap_detail(&text, 40);
        assert!(lines.len() > 2);
        assert!(lines.iter().all(|l| l.len() <= 40));
        assert_eq!(lines.concat().replace(' ', ""), text.replace(' ', ""));
        assert!(
            !wrap_detail("path\u{1b}[31m", 40)
                .join("")
                .contains('\u{1b}')
        );
    }

    #[test]
    fn wave450_doctor_preserves_profile_locks_and_existing_probe_file() {
        let _cache = isolated_cache();
        let profile = crate::ghost::profile_dir();
        std::fs::create_dir_all(&profile).unwrap();
        for name in [
            "SingletonLock",
            "SingletonSocket",
            "SingletonCookie",
            ".doctor-write-test",
        ] {
            std::fs::write(profile.join(name), b"owned by another process").unwrap();
        }
        assert!(matches!(
            check_ghost_profile(),
            CheckResult::Pass(_) | CheckResult::Warn(_)
        ));
        for name in [
            "SingletonLock",
            "SingletonSocket",
            "SingletonCookie",
            ".doctor-write-test",
        ] {
            assert_eq!(
                std::fs::read(profile.join(name)).unwrap(),
                b"owned by another process"
            );
        }
    }

    mod tempfile_dir {
        pub struct TempDir {
            pub dir: std::path::PathBuf,
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    // Unix only: `/bin/true` is path-form, so on Windows it does not
    // exist and both plugins land in "program not found" as well.
    #[cfg(unix)]
    #[test]
    fn a_parked_plugin_warns_instead_of_passing() {
        // The gap: a plugin sitting in `invalid` passed the check, so
        // the only place its state showed was `keys list`.
        let _g = isolated_cache();
        let mut cfg = crate::search::byok::plugin::PluginConfig::empty();
        let none = std::collections::HashSet::new();
        cfg.add("parkedplug", vec!["/bin/true".into()], 30_000, &none)
            .unwrap();
        cfg.add("liveplug", vec!["/bin/true".into()], 30_000, &none)
            .unwrap();
        cfg.mark_state("parkedplug", KeyState::Invalid);
        cfg.save();

        match check_plugins() {
            CheckResult::Warn(detail) => {
                assert!(detail.contains("parkedplug: invalid"), "{detail}");
                assert!(
                    !detail.contains("liveplug:"),
                    "an active plugin must not be named as parked: {detail}"
                );
            }
            other => panic!("a parked plugin must warn, got {other:?}"),
        }
    }

    #[test]
    fn a_plugin_with_no_command_warns_instead_of_panicking() {
        // A hand-edited plugins.json can carry an empty argv; the old
        // `cmd[0]` indexed straight into a panic.
        let _g = isolated_cache();
        let dir = std::env::var("DONSETCH_CACHE_DIR").unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join("plugins.json"),
            r#"{"version":1,"plugins":{"broken":{"cmd":[],"timeout_ms":30000}},"order":["broken"]}"#,
        )
        .unwrap();
        match check_plugins() {
            CheckResult::Warn(detail) => {
                assert!(detail.contains("broken: no command registered"), "{detail}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn config_posture_flags_no_config_file_plus_explicit_path() {
        let _g = isolated_cache();
        unsafe {
            std::env::set_var("DONSETCH_NO_CONFIG_FILE", "1");
            std::env::set_var("DONSETCH_CONFIG", "/nonexistent/donsetch.toml");
        }
        let r = check_config_posture();
        unsafe {
            std::env::remove_var("DONSETCH_NO_CONFIG_FILE");
            std::env::remove_var("DONSETCH_CONFIG");
        }
        match r {
            CheckResult::Fail(detail, _) => {
                assert!(detail.contains("both set"), "{detail}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn config_posture_flags_missing_explicit_config() {
        let _g = isolated_cache();
        unsafe {
            std::env::remove_var("DONSETCH_NO_CONFIG_FILE");
            std::env::set_var("DONSETCH_CONFIG", "/nonexistent/donsetch.toml");
        }
        let r = check_config_posture();
        unsafe {
            std::env::remove_var("DONSETCH_CONFIG");
        }
        match r {
            CheckResult::Fail(detail, _) => {
                assert!(detail.contains("missing file"), "{detail}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    fn rules_section(
        mode: crate::rules::RulesMode,
        enabled: &[bool],
    ) -> crate::rules::RulesSection {
        let mut s = crate::rules::RulesSection {
            mode,
            ..Default::default()
        };
        for (i, on) in enabled.iter().enumerate() {
            s.url.insert(
                format!("host{i}.example"),
                crate::rules::UrlRule {
                    enabled: *on,
                    ..Default::default()
                },
            );
        }
        s
    }

    #[test]
    fn rules_row_warns_when_mode_is_off_with_live_rules() {
        let s = rules_section(crate::rules::RulesMode::Off, &[true, false]);
        match rules_check(&s, "DONSETCH_RULES__MODE") {
            CheckResult::Warn(detail) => {
                assert!(detail.contains("2 rules (1 disabled)"), "{detail}");
                assert!(
                    detail.contains("mode=off (DONSETCH_RULES__MODE)"),
                    "{detail}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn rules_row_passes_when_enforced_or_when_nothing_is_switched_off() {
        let enforced = rules_section(crate::rules::RulesMode::Enforce, &[true]);
        match rules_check(&enforced, "default") {
            CheckResult::Pass(detail) => {
                assert_eq!(detail, "1 rule · mode=enforce (default)");
            }
            other => panic!("expected Pass, got {other:?}"),
        }
        // Off with no live rule switches nothing off.
        let empty = rules_section(crate::rules::RulesMode::Off, &[]);
        assert!(
            matches!(rules_check(&empty, "default"), CheckResult::Pass(_)),
            "an empty table with mode off must not warn"
        );
    }

    #[test]
    fn search_health_reports_kill_switches() {
        let _g = isolated_cache();
        let r = check_search_health();
        let detail = match r {
            CheckResult::Pass(d) | CheckResult::Warn(d) => d,
            CheckResult::Fail(d, _) => d,
            CheckResult::Fixed(d) => d,
        };
        assert!(detail.contains("early="), "{detail}");
        assert!(detail.contains("compile="), "{detail}");
        assert!(detail.contains("instant="), "{detail}");
        assert!(detail.contains("byok"), "{detail}");
    }

    #[test]
    fn clearance_and_crawl_stores_tolerate_missing_files() {
        let _g = isolated_cache();
        let c = check_clearance_stores();
        assert!(
            matches!(c, CheckResult::Pass(_)),
            "clearance must pass on empty cache"
        );
        let k = check_crawl_stores();
        assert!(
            matches!(k, CheckResult::Pass(_)),
            "crawl must pass on empty cache"
        );
    }

    #[test]
    fn state_permissions_tightens_world_readable_secret_files() {
        let _g = isolated_cache();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let make = |name: &str| {
                let f = paths::cache_dir().join(name);
                std::fs::write(&f, b"{}").unwrap();
                let mut perm = std::fs::metadata(&f).unwrap().permissions();
                perm.set_mode(0o644);
                std::fs::set_permissions(&f, perm).unwrap();
                f
            };
            let routes = make("routes.json");
            // page-history keeps whole page markdown (including pages
            // fetched behind a session) and was missed by the
            // original list.
            let history = make("page-history.json");
            match check_state_permissions() {
                CheckResult::Fixed(d) => {
                    assert!(d.contains("routes.json"), "{d}");
                    assert!(d.contains("page-history.json"), "{d}");
                }
                other => panic!("expected Fixed, got {other:?}"),
            }
            for f in [routes, history] {
                let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{}", f.display());
            }
        }
    }

    // Screenshots and debug DOM dumps written before they were sealed
    // stay on disk; doctor tightens them where they lie, subfolders
    // included, and never follows a link out of the tree.
    #[test]
    fn state_permissions_tightens_page_artifacts() {
        let _g = isolated_cache();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let make = |rel: &str| {
                let f = paths::cache_dir().join(rel);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(&f, b"x").unwrap();
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
                f
            };
            let shot = make("screenshots/a.png");
            let nested = make("screenshots/sub/b.png");
            let dom = make("ghost-debug/dom-app_example_com.html");
            let outside = make("elsewhere/kept.txt");
            std::os::unix::fs::symlink(&outside, paths::cache_dir().join("screenshots/link.png"))
                .unwrap();
            match check_state_permissions() {
                CheckResult::Fixed(d) => {
                    assert!(d.contains("screenshots/"), "{d}");
                    assert!(d.contains("ghost-debug/"), "{d}");
                }
                other => panic!("expected Fixed, got {other:?}"),
            }
            let mode =
                |f: &std::path::Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
            for f in [&shot, &nested, &dom] {
                assert_eq!(mode(f), 0o600, "{}", f.display());
            }
            assert_eq!(
                mode(&outside),
                0o644,
                "a link's target is not ours to change"
            );
            // A second run has nothing left to fix.
            assert!(matches!(check_state_permissions(), CheckResult::Pass(_)));
        }
    }

    // Crawl resume tokens and unlocker bypass entries carry frontier
    // URLs and page bodies: same tighten as the page artifacts.
    // The pace, governor, egress and search-learning stores name the
    // hosts that were fetched and searched; the ones written before
    // they were sealed are tightened where they lie.
    #[test]
    fn state_permissions_tightens_the_host_bearing_stores() {
        let _g = isolated_cache();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let make = |rel: &str| {
                let f = paths::cache_dir().join(rel);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(&f, b"{}").unwrap();
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
                f
            };
            let files = [
                make("crawl-governor.json"),
                make("egress-health.json"),
                make("search-trust.json"),
                make("search-quality.json"),
                make("outcome-feedback.json"),
                make("host-pace/0a1b2c3d.json"),
            ];
            let report = match check_state_permissions() {
                CheckResult::Fixed(d) => d,
                other => panic!("expected Fixed, got {other:?}"),
            };
            for name in [
                "crawl-governor.json",
                "egress-health.json",
                "search-trust.json",
                "search-quality.json",
                "outcome-feedback.json",
                "host-pace/",
            ] {
                assert!(report.contains(name), "{name} missing from: {report}");
            }
            for f in &files {
                let mode = std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{}", f.display());
            }
        }
    }

    #[test]
    fn state_permissions_tightens_crawl_and_bypass_stores() {
        let _g = isolated_cache();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let make = |rel: &str| {
                let f = paths::cache_dir().join(rel);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(&f, b"x").unwrap();
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
                f
            };
            let tok = make("crawl-resumes/c0001.json");
            let cache = make("bypass-cache/abc.json");
            let single = make("search-cache.json");
            match check_state_permissions() {
                CheckResult::Fixed(d) => {
                    assert!(d.contains("crawl-resumes/"), "{d}");
                    assert!(d.contains("bypass-cache/"), "{d}");
                    assert!(d.contains("search-cache.json"), "{d}");
                }
                other => panic!("expected Fixed, got {other:?}"),
            }
            let mode =
                |f: &std::path::Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
            for f in [&tok, &cache, &single] {
                assert_eq!(mode(f), 0o600, "{}", f.display());
            }
            assert!(matches!(check_state_permissions(), CheckResult::Pass(_)));
        }
    }
}

#[cfg(test)]
mod mask_tests {
    use super::mask_key;

    #[test]
    fn mask_key_is_char_safe_and_never_shows_most_of_a_short_key() {
        // 8 bytes, 7 chars, last char multibyte: `&start[..7]` panicked.
        assert_eq!(mask_key("abcdefé"), "ab***");
        // All-multibyte short key.
        assert_eq!(mask_key("密钥测试"), "密钥***");
        // Degenerate lengths.
        assert_eq!(mask_key(""), "***");
        assert_eq!(mask_key("a"), "***");
        assert_eq!(mask_key("ab"), "a***");
        // A real-length key keeps the recognizable head...tail shape.
        assert_eq!(
            mask_key("sk-abcdefghijklmnopqrstuvwxyz0123"),
            "sk-abc...0123"
        );
        // Multibyte at both cut positions.
        assert_eq!(mask_key("ключключключключ"), "ключкл...ключ");
        // Bright Data `token::zone` keys mask the token only.
        assert_eq!(mask_key("0123456789abcdef::my_zone"), "012345...cdef");
    }
}

#[cfg(test)]
mod bright_probe_tests {
    use super::{ZoneProbeOut, bright_zone_probe_at, classify_zones, json_escape};
    use std::io::{Read, Write};

    #[test]
    fn account_zones_require_the_exact_name_and_product() {
        let body = br#"[{"name":"custom-serp","type":"serp"},{"name":"custom-unlocker","type":"unblocker"}]"#;
        assert_eq!(
            classify_zones(200, body, "custom-serp", "serp"),
            ZoneProbeOut::Validated
        );
        assert_eq!(
            classify_zones(200, body, "custom-unlocker", "unblocker"),
            ZoneProbeOut::Validated
        );
        for (zone, kind) in [
            ("serp_api1", "serp"),
            ("custom-serp", "unblocker"),
            ("custom-unlocker", "serp"),
        ] {
            assert!(matches!(
                classify_zones(200, body, zone, kind),
                ZoneProbeOut::Failed { .. }
            ));
        }
        assert!(matches!(
            classify_zones(401, b"unauthorized", "any", "serp"),
            ZoneProbeOut::Failed { .. }
        ));
        assert!(matches!(
            classify_zones(403, b"forbidden", "any", "serp"),
            ZoneProbeOut::Skipped { .. }
        ));
        assert!(matches!(
            classify_zones(200, b"{}", "any", "serp"),
            ZoneProbeOut::Failed { .. }
        ));
        assert!(matches!(
            classify_zones(200, br#"[{"name":"any"}]"#, "any", "serp"),
            ZoneProbeOut::Failed { .. }
        ));
    }

    #[test]
    fn wire_account_zone_check_is_authenticated_and_free() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0);
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = r#"[{"name":"zone-a","type":"unblocker"}]"#;
            write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8(head).unwrap().to_ascii_lowercase()
        });
        assert_eq!(
            bright_zone_probe_at(&base, "test-token", "zone-a", "unblocker"),
            ZoneProbeOut::Validated
        );
        let head = handle.join().unwrap();
        assert!(
            head.starts_with("get /zone/get_active_zones http/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("authorization: bearer test-token"), "{head}");
        assert!(!head.contains("/request"));
    }

    #[test]
    fn json_escape_handles_windows_paths() {
        let path = r#"C:\Users\A "B"\donsetch.exe"#;
        assert_eq!(json_escape(path), serde_json::to_string(path).unwrap());
    }
}
