//! `donsetch status` : one-glance overview of everything.
//!
//! Shows: version + update check, keys (count + default mode),
//! proxies (count), cache size, quick health hint. This is the
//! "I just installed it, what's the state?" command : fast, no
//! network probes, no browser launch.

use crate::DISPLAY_NAME;
use crate::cli;
use crate::fetch::client::Fetcher;
use crate::profile::BrowserProfile;
use crate::search::byok::store::ByokConfig;
use crate::transport::proxy;

const REPO: &str = "dondai44423/donsetch";

pub async fn run() {
    cli::init();
    let current = env!("CARGO_PKG_VERSION");
    cli::print_title(&format!("{DISPLAY_NAME} {current} : Status"));
    println!();

    // ── Version + update ────────────────────────────────────

    let fetcher = Fetcher::new(BrowserProfile::host_default());
    let latest = match &fetcher {
        Ok(f) => fetch_latest_version(f).await.ok(),
        Err(_) => None,
    };

    match &latest {
        Some(latest) => {
            let cur = semver::Version::parse(current).ok();
            let lat = semver::Version::parse(latest).ok();
            match (cur, lat) {
                (Some(c), Some(l)) if c == l => {
                    cli::print_kv("version", &format!("{} (up to date)", cli::green(current)));
                }
                (Some(c), Some(l)) if c > l => {
                    cli::print_kv(
                        "version",
                        &format!("{} (ahead of {latest})", cli::green(current)),
                    );
                }
                _ => {
                    cli::print_kv(
                        "version",
                        &format!("{} (update available: {latest})", cli::yellow(current)),
                    );
                }
            }
        }
        None => {
            cli::print_kv("version", &format!("{} (offline)", cli::green(current)));
        }
    }

    // ── Search (BYOK keys) ───────────────────────────────────

    let cfg = ByokConfig::load();
    if cfg.is_configured() {
        let n_keys: usize = cfg.providers.iter().map(|p| p.keys.len()).sum();
        let mode = if cfg.is_local_default() {
            format!("{} (local-first, BYOK fallback)", cli::green("local"))
        } else if !cfg.default.is_empty() {
            format!("{} (BYOK-first, local fallback)", cli::green(&cfg.default))
        } else {
            format!("{} (BYOK-first, local fallback)", cli::green("auto"))
        };
        cli::print_kv(
            "search",
            &format!(
                "{} provider(s), {} key(s), default: {}",
                cfg.providers.len(),
                n_keys,
                mode
            ),
        );
    } else {
        cli::print_kv(
            "search",
            &format!("{} (no BYOK keys, local engine)", cli::green("local")),
        );
    }

    // ── Proxies ──────────────────────────────────────────────

    let proxies = proxy::load_config();
    if proxies.is_empty() {
        cli::print_kv(
            "proxies",
            &format!("{} (direct connection)", cli::dim("none")),
        );
    } else {
        let n = proxies.len();
        let word = if n == 1 { "proxy" } else { "proxies" };
        cli::print_kv("proxies", &format!("{} {} configured", n, word));
    }

    // ── Cache ────────────────────────────────────────────────

    let cache = crate::paths::cache_dir();
    let cache_size = if cache.exists() {
        format_size(dir_size(&cache))
    } else {
        "not created yet".to_string()
    };
    cli::print_kv("cache", &cache_size);

    // Resolution may probe a browser; keep that blocking operation off the
    // async executor. Status never triggers a public binary download.
    let browser = tokio::task::spawn_blocking(crate::ghost::resolve_browser_without_download)
        .await
        .unwrap_or_else(|e| {
            Err(crate::error::FetchError::ghost(format!(
                "browser resolution task failed: {e}"
            )))
        });
    match &browser {
        Ok(info) => {
            cli::print_kv("browser", &info.describe());
        }
        Err(error) => {
            cli::print_kv("browser", &format!("unavailable: {error}"));
        }
    }

    let ghost_state = cache.join("ghost-state.json");
    let has_state = ghost_state.exists();
    let domains = if has_state {
        crate::ghost::cache::GhostState::load().profiles.len()
    } else {
        0
    };

    let health = if browser.is_ok() && domains > 0 {
        format!(
            "{} browser ready, {} domain profile(s)",
            cli::green("good"),
            domains
        )
    } else if browser.is_ok() {
        format!("{} browser ready, no profiles yet", cli::green("good"))
    } else {
        format!(
            "{} browser unavailable: tier 2 unavailable",
            cli::red("warn")
        )
    };
    cli::print_kv("health", &health);
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
