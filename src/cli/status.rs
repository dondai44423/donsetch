//! `donsetch status` : one-glance overview of everything.
//!
//! Shows: version + update check, keys (count + default mode),
//! proxies (count), cache size, quick health hint. This is the
//! "I just installed it, what's the state?" command : fast, no
//! network probes, no browser launch.
//!
//! `--json` emits the same data as one JSON object for scripts and
//! agents: the human view is a rendering of these fields, so the two
//! cannot disagree. Unlike `doctor --json` (which appends its document
//! to a long human report), the JSON here is the whole of stdout, so
//! `donsetch status --json | jq .status.version` works.

use crate::DISPLAY_NAME;
use crate::cli;
use crate::egress::proxy;
use crate::fetch::client::Fetcher;
use crate::profile::BrowserProfile;
use crate::search::byok::store::ByokConfig;
use serde_json::json;

const REPO: &str = "dondai44423/donsetch";

/// Usage text, shared by `donsetch status --help` and `donsetch help status`.
pub fn help() {
    println!("Usage: donsetch status [--json]");
    println!();
    println!("  Shows a quick overview: version, search config, proxies, cache, health.");
    println!("  No probes, no browser launch : fast.");
    println!("  For full diagnostics, run `donsetch doctor`.");
    println!();
    println!("  --json   The same data as one JSON object on stdout, for scripts");
    println!("           and agents (the human view renders the same fields).");
}

pub async fn run(args: &[String]) {
    cli::args::Spec::new("status", "donsetch status [--json]", &["--json"]).reject(&args[2..]);
    let as_json = args[2..].iter().any(|a| a == "--json");
    cli::init();
    let current = env!("CARGO_PKG_VERSION");

    // ── Version + update ────────────────────────────────────

    let fetcher = Fetcher::new(BrowserProfile::host_default());
    let latest = match &fetcher {
        Ok(f) => fetch_latest_version(f).await.ok(),
        Err(_) => None,
    };

    // "update state" is the machine value; the human line renders it.
    let update_state = match &latest {
        Some(latest) => {
            let cur = semver::Version::parse(current).ok();
            let lat = semver::Version::parse(latest).ok();
            match (cur, lat) {
                (Some(c), Some(l)) if c == l => "up_to_date",
                (Some(c), Some(l)) if c > l => "ahead",
                _ => "update_available",
            }
        }
        None => "offline",
    };
    let version_human = match (update_state, latest.as_deref()) {
        ("up_to_date", _) => format!("{} (up to date)", cli::green(current)),
        ("ahead", Some(latest)) => format!("{} (ahead of {latest})", cli::green(current)),
        ("update_available", Some(latest)) => {
            format!("{} (update available: {latest})", cli::yellow(current))
        }
        _ => format!("{} (offline)", cli::green(current)),
    };

    // ── Search (BYOK keys) ───────────────────────────────────

    let cfg = ByokConfig::load();
    let n_keys: usize = cfg.providers.iter().map(|p| p.keys.len()).sum();
    let (search_mode, search_default, search_human) = if cfg.is_configured() {
        let (mode, default, rendered) = if cfg.is_local_default() {
            (
                "local",
                None,
                format!("{} (local-first, BYOK fallback)", cli::green("local")),
            )
        } else if !cfg.default.is_empty() {
            (
                "byok_first",
                Some(cfg.default.clone()),
                format!("{} (BYOK-first, local fallback)", cli::green(&cfg.default)),
            )
        } else {
            (
                "byok_first",
                None,
                format!("{} (BYOK-first, local fallback)", cli::green("auto")),
            )
        };
        (
            mode,
            default,
            format!(
                "{} provider(s), {} key(s), default: {}",
                cfg.providers.len(),
                n_keys,
                rendered
            ),
        )
    } else {
        (
            "local",
            None,
            format!("{} (no BYOK keys, local engine)", cli::green("local")),
        )
    };

    // ── Proxies ──────────────────────────────────────────────

    let (proxies, skipped_proxy_lines) = proxy::load_config_verbose();
    let proxies_human = if proxies.is_empty() {
        if skipped_proxy_lines > 0 {
            format!(
                "{} ({} invalid line(s) ignored in {})",
                cli::yellow("warn"),
                skipped_proxy_lines,
                cli::dim("proxies.txt")
            )
        } else {
            format!("{} (direct connection)", cli::dim("none"))
        }
    } else {
        let n = proxies.len();
        let word = if n == 1 { "proxy" } else { "proxies" };
        let skipped = if skipped_proxy_lines > 0 {
            format!(", {} invalid line(s) ignored", skipped_proxy_lines)
        } else {
            String::new()
        };
        format!("{} {} configured{}", n, word, skipped)
    };

    // ── Cache ────────────────────────────────────────────────

    let cache = crate::paths::cache_dir();
    let cache_bytes = cache.exists().then(|| dir_size(&cache));
    let cache_human = match cache_bytes {
        Some(bytes) => format_size(bytes),
        None => "not created yet".to_string(),
    };

    // Resolution may probe a browser; keep that blocking operation off the
    // async executor. Status never triggers a public binary download.
    let browser = tokio::task::spawn_blocking(crate::ghost::resolve_browser_without_download)
        .await
        .unwrap_or_else(|e| {
            Err(crate::error::FetchError::ghost(format!(
                "browser resolution task failed: {e}"
            )))
        });
    let browser_human = match &browser {
        Ok(info) => info.describe(),
        Err(error) => format!("unavailable: {error}"),
    };
    let browser_json = match &browser {
        Ok(info) => json!({
            "available": true,
            "backend": info.backend.as_str(),
            "source": info.source,
            "path": info.path.display().to_string(),
            "version": info.version,
        }),
        Err(error) => json!({ "available": false, "detail": error.to_string() }),
    };

    let ghost_state = cache.join("ghost-state.json");
    let has_state = ghost_state.exists();
    let domains = if has_state {
        crate::ghost::cache::GhostState::load().profiles.len()
    } else {
        0
    };

    let (health_state, health_human) = if browser.is_ok() && domains > 0 {
        (
            "good",
            format!(
                "{} browser ready, {} domain profile(s)",
                cli::green("good"),
                domains
            ),
        )
    } else if browser.is_ok() {
        (
            "good",
            format!("{} browser ready, no profiles yet", cli::green("good")),
        )
    } else {
        (
            "warn",
            format!(
                "{} browser unavailable: screenshot, actions and login will not work",
                cli::red("warn")
            ),
        )
    };

    // v4 phase 0: route-memory visibility (the self-improvement
    // loop must be observable, never magic).
    let route_memory_off =
        crate::config::cfg().state.route_memory == crate::config::RouteMemory::Off;
    let route_read_only =
        crate::config::cfg().state.route_memory == crate::config::RouteMemory::ReadOnly;
    let state = crate::ghost::cache::GhostState::load();
    let (hosts, walled, warm, cooldowns, flaky) = state.route_stats();
    let route_line = if route_memory_off {
        "off (state.route_memory = off)".to_string()
    } else {
        let ro = if route_read_only { " · read-only" } else { "" };
        if hosts == 0 {
            format!("learning (no hosts yet){ro}")
        } else {
            let mut line = format!("{hosts} hosts · {walled} walled · {warm} warm");
            if !state.personas.is_empty() {
                line.push_str(&format!(" · {} personas", state.personas.len()));
            }
            if state.probes_total > 0 {
                line.push_str(&format!(" · {} probes", state.probes_total));
            }
            if state.tier1_cookie_count() > 0 {
                line.push_str(&format!(" · {} vaulted", state.tier1_cookie_count()));
            }
            if state.shadowed_assets_total > 0 {
                line.push_str(&format!(" · {} shadowed", state.shadowed_assets_total));
            }
            if state.prewarmed_served_total > 0 {
                line.push_str(&format!(" · {} prewarmed", state.prewarmed_served_total));
            }
            if state.pool_served_total > 0 {
                line.push_str(&format!(" · {} warm", state.pool_served_total));
            }
            if cooldowns > 0 {
                line.push_str(&format!(" · {cooldowns} in cooldown"));
            }
            if flaky > 0 {
                line.push_str(&format!(" · {flaky} flaky-wall"));
            }
            line.push_str(ro);
            line
        }
    };

    // v4 B5 Improve receipts: the self-improvement loop must be
    // observable, never magic. Warm-hit + cooldown + trust health.
    let warm_hits = state.pool_served_total + state.prewarmed_served_total;
    let low_trust = {
        // Local search-trust snapshot without spinning a Searcher.
        let (t, ti, f) = crate::search::persist_load_for_status();
        t.values().filter(|&&x| x < 0.5).count()
            + ti.values().filter(|&&x| x < 0.5).count()
            + f.len()
    };
    let (q_hosts, q_high, q_low) = crate::search::persist_load_quality_for_status();
    let (o_keys, o_demoted) = crate::search::persist_load_outcome_for_status();
    let improve_human = if hosts == 0
        && warm_hits == 0
        && low_trust == 0
        && q_hosts == 0
        && o_keys == 0
    {
        "learning (nothing recorded yet)".to_string()
    } else {
        format!(
            "{warm} warm-ready · {warm_hits} warm-hits · {walled} walled · {cooldowns} cooldowns · {flaky} flaky · {low_trust} low-trust/quarantined · quality {q_high}+/{q_low}-/{q_hosts} · outcome {o_demoted}/{o_keys}"
        )
    };

    if as_json {
        let doc = json!({
            "status": {
                "version": current,
                "latest": latest,
                "update_state": update_state,
                "search": {
                    "mode": search_mode,
                    "providers": cfg.providers.len(),
                    "keys": n_keys,
                    "default": search_default,
                },
                "proxies": { "count": proxies.len(), "skipped_lines": skipped_proxy_lines },
                "cache_bytes": cache_bytes,
                "browser": browser_json,
                "profiles": domains,
                "health": { "state": health_state },
                "route_memory": {
                    "enabled": !route_memory_off,
                    "read_only": route_read_only,
                    "hosts": hosts,
                    "walled": walled,
                    "warm_lanes": warm,
                    "cooldowns": cooldowns,
                    "flaky": flaky,
                    "personas": state.personas.len(),
                    "probes": state.probes_total,
                    "vaulted": state.tier1_cookie_count(),
                },
                "improve": {
                    "warm_ready": warm,
                    "warm_hits": warm_hits,
                    "low_trust": low_trust,
                    "quality_high": q_high,
                    "quality_low": q_low,
                    "quality_hosts": q_hosts,
                    "outcome_demoted": o_demoted,
                    "outcome_keys": o_keys,
                },
                "mcp": cli::mcp_output_status(),
                "deep_fingerprint": "not_probed",
            }
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return;
    }

    cli::print_title(&format!("{DISPLAY_NAME} {current} : Status"));
    println!();
    cli::print_kv("version", &version_human);
    cli::print_kv("search", &search_human);
    cli::print_kv("MCP output", &cli::mcp_output_summary());
    cli::print_kv("proxies", &proxies_human);
    cli::print_kv("cache", &cache_human);
    cli::print_kv("browser", &browser_human);
    cli::print_kv("health", &health_human);
    cli::print_kv("route memory", &route_line);
    cli::print_kv("improve", &improve_human);
    cli::print_kv(
        "deep fingerprint",
        "not probed (run `donsetch doctor --deep`)",
    );

    println!();
    println!(
        "  {} Run {} for full diagnostics.",
        cli::dim("tip:"),
        cli::bold("donsetch doctor")
    );
    cli::print_footer();
}

// ── Helpers ──────────────────────────────────────────────────

fn dir_size(path: &std::path::Path) -> u64 {
    fn walk(path: &std::path::Path, total: &mut u64) {
        if *total > 2_000_000_000 {
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

/// Fetch the latest version from the GitHub releases.atom feed.
/// Same logic as version.rs : no API, no rate limits.
async fn fetch_latest_version(fetcher: &Fetcher) -> Result<String, String> {
    let url = format!("https://github.com/{REPO}/releases.atom");
    let out = fetcher.fetch(&url).await.map_err(|e| e.to_string())?;

    if out.status != 200 {
        return Err(format!("HTTP {} from releases feed", out.status));
    }

    let body = String::from_utf8_lossy(&out.body);

    let entry_pos = body
        .find("<entry>")
        .ok_or_else(|| "no releases found in feed".to_string())?;

    let id_tag = body[entry_pos..]
        .find("<id>")
        .ok_or_else(|| "could not parse feed: no <id> in first entry".to_string())?
        + entry_pos;

    let content_start = body[id_tag..]
        .find('>')
        .ok_or_else(|| "could not parse feed: malformed <id>".to_string())?
        + id_tag
        + 1;

    let content_end = body[content_start..]
        .find("</id>")
        .ok_or_else(|| "could not parse feed: no </id>".to_string())?
        + content_start;

    let id_content = body[content_start..content_end].trim();
    let tag = id_content.rsplit('/').next().unwrap_or(id_content);
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Ok(version.to_string())
}
