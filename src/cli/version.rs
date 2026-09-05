//! `donsetch -v` / `donsetch --version` : build identity + update check.
//!
//! Shows build info (binary, target, profile, features, pdfium, git).
//! Then fetches the latest release tag from the GitHub releases.atom
//! feed (no API key, no rate limits) and shows whether the current
//! version is up to date.

use crate::DISPLAY_NAME;
use crate::cli;
use crate::fetch::client::Fetcher;
use crate::profile::BrowserProfile;

const REPO: &str = "dondai44423/donsetch";

pub async fn run() {
    crate::cli::init();

    let current = env!("CARGO_PKG_VERSION");
    cli::print_title(&format!("{DISPLAY_NAME} {current}"));

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    cli::print_kv("binary", &exe);
    cli::print_kv(
        "target",
        option_env!("DONSHEET_TARGET").unwrap_or("unknown"),
    );
    cli::print_kv("profile", "chrome-150");
    cli::print_kv(
        "features",
        option_env!("DONSHEET_FEATURES").unwrap_or("(none)"),
    );
    cli::print_kv(
        "pdfium",
        option_env!("DONSHEET_PDFIUM").unwrap_or("unknown"),
    );
    cli::print_kv("git", option_env!("DONSHEET_GIT_HASH").unwrap_or("unknown"));

    // ── Update check (atom feed : no API, no rate limits) ─────
    //
    // Fetches the GitHub releases.atom feed (a regular web page,
    // not the API : no 60-req/hour limit). Parses the first
    // <entry><title> for the latest tag. Compares semver.

    let fetcher = Fetcher::new(BrowserProfile::host_default());
    let latest = match fetcher {
        Ok(f) => fetch_latest_version(&f).await.ok(),
        Err(_) => None,
    };

    println!();

    match &latest {
        Some(latest) => {
            let cur = semver::Version::parse(current).ok();
            let lat = semver::Version::parse(latest).ok();
            match (cur, lat) {
                (Some(c), Some(l)) if c == l => {
                    println!("  {} up to date ({current})", cli::icon_pass());
                }
                (Some(c), Some(l)) if c > l => {
                    println!(
                        "  {} ahead of latest ({current} > {latest})",
                        cli::icon_warn(),
                    );
                }
                _ => {
                    println!(
                        "  {} update available: {current} → {latest}",
                        cli::icon_warn(),
                    );
                    println!("    Run `donsetch update` to upgrade.");
                }
            }
        }
        None => {
            println!(
                "  {} could not check for updates (offline?)",
                cli::icon_warn()
            );
        }
    }
}

/// Fetch the releases.atom feed and parse the latest release tag.
/// Same logic as `cli::update::fetch_latest_version` : duplicated
/// here to keep `version` independent of `update` (no cross-module
/// dependency for a one-off atom parse).
///
/// Uses the `<id>` tag (not `<title>`) because release titles can
/// contain extra text (e.g. "v1.0.0 : Stable Release") that breaks
/// semver parsing. The `<id>` tag always ends with `/v<version>`.
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

    // The <id> tag always ends with /v<version> : clean, no extra text.
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

    // <id> looks like: tag:github.com,2008:Repository/123/v1.0.0
    // Extract everything after the last '/'.
    let id_content = body[content_start..content_end].trim();
    let tag = id_content.rsplit('/').next().unwrap_or(id_content);

    let version = tag.strip_prefix('v').unwrap_or(tag);
    Ok(version.to_string())
}
