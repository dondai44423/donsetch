//! `donsetch -v` / `donsetch --version` : build identity + update check.
//!
//! Shows build info (binary, target, profile, features, pdfium, git).
//! Then fetches the latest release tag from the GitHub releases.atom
//! feed (no API key, no rate limits) and shows whether the current
//! version is up to date.

use crate::DISPLAY_NAME;
use crate::cli;
use crate::cli::update::{self, Offer};
use crate::fetch::client::Fetcher;
use crate::profile::BrowserProfile;
use std::time::Duration;

pub async fn run(args: &[String]) {
    crate::cli::args::Spec::new("version", "donsetch version", &[]).reject(&args[2..]);
    crate::cli::init();

    let current = env!("CARGO_PKG_VERSION");
    cli::print_title(&format!("{DISPLAY_NAME} {current}"));

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    cli::print_kv("binary", &exe);
    cli::print_kv(
        "target",
        option_env!("DONSETCH_TARGET").unwrap_or("unknown"),
    );
    // The label must name the Chrome this build actually presents. The UA
    // and the client hints follow the detected browser, so a literal here
    // mislabelled every build (and every recorded stealth baseline) as the
    // version the tables were captured from. When no browser can be
    // detected the profile falls back to the capture version, and that is
    // said out loud instead of presented as a fact about the host.
    let profile = BrowserProfile::host_default();
    let label = match crate::profile::probe_installed() {
        Some(_) => format!("{} (detected)", profile.name),
        None => format!("{} (fallback: no installed browser detected)", profile.name),
    };
    cli::print_kv("profile", &label);
    cli::print_kv(
        "features",
        option_env!("DONSETCH_FEATURES").unwrap_or("(none)"),
    );
    cli::print_kv(
        "pdfium",
        option_env!("DONSETCH_PDFIUM").unwrap_or("unknown"),
    );
    cli::print_kv("git", option_env!("DONSETCH_GIT_HASH").unwrap_or("unknown"));

    // ── Update check (atom feed : no API, no rate limits) ─────
    //
    // Fetches the GitHub releases.atom feed (a regular web page,
    // not the API : no 60-req/hour limit). The verdict comes from
    // update::check_feed, the one `donsetch update` acts on.

    let skip_update_check =
        std::env::var_os("DONSETCH_NO_UPDATE_CHECK").is_some() || std::env::var_os("CI").is_some();
    let feed = if skip_update_check {
        None
    } else {
        let fetcher = Fetcher::new(BrowserProfile::host_default());
        match fetcher {
            Ok(f) => tokio::time::timeout(Duration::from_secs(3), update::fetch_feed(&f))
                .await
                .ok()
                .and_then(Result::ok),
            Err(_) => None,
        }
    };

    println!();

    if skip_update_check {
        println!("  (update check skipped)");
        return;
    }

    println!("{}", update_report(current, feed.as_deref()));
}

/// The update lines `version` prints for an install at `current`,
/// from a releases.atom `feed` body (None: no answer). They announce
/// an update only when `donsetch update` would install it.
fn update_report(current: &str, feed: Option<&str>) -> String {
    match feed.map(|body| update::check_feed(body, current)) {
        Some(Ok((_, Offer::UpToDate))) => format!("  {} up to date ({current})", cli::icon_pass()),
        Some(Ok((latest, Offer::Prerelease))) => format!(
            "  {} up to date ({current}); latest published is a prerelease ({latest})",
            cli::icon_pass()
        ),
        Some(Ok((latest, Offer::Ahead))) => format!(
            "  {} ahead of latest ({current} > {latest})",
            cli::icon_warn()
        ),
        Some(Ok((latest, Offer::Newer))) => format!(
            "  {} update available: {current} → {latest}\n    Run `donsetch update` to upgrade.",
            cli::icon_warn()
        ),
        Some(Err(_)) | None => format!(
            "  {} could not check for updates (offline?)",
            cli::icon_warn()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::update::tests::feed;

    // `update` keeps a stable install on the newest stable release
    // when a prerelease tops the feed, so `version` must not announce
    // the prerelease as an update.
    #[test]
    fn a_stable_install_is_not_told_to_update_to_a_prerelease() {
        let body = feed(&["v5.1.0-beta.1", "v5.0.0", "v4.10.5"]);
        let report = update_report("5.0.0", Some(body.as_str()));
        assert!(report.contains("up to date (5.0.0)"), "{report}");
        assert!(!report.contains("update available"), "{report}");
        let body = feed(&["v5.1.0-beta.1", "v5.0.1", "v5.0.0"]);
        let report = update_report("5.0.0", Some(body.as_str()));
        assert!(
            report.contains("update available: 5.0.0 → 5.0.1"),
            "{report}"
        );
        let body = feed(&["v5.1.0-beta.2", "v5.1.0-beta.1"]);
        let report = update_report("5.0.0", Some(body.as_str()));
        assert!(!report.contains("update available"), "{report}");
    }

    #[test]
    fn a_newer_release_the_install_follows_is_announced() {
        let body = feed(&["v5.0.1", "v5.0.0"]);
        let report = update_report("5.0.0", Some(body.as_str()));
        assert!(
            report.contains("update available: 5.0.0 → 5.0.1"),
            "{report}"
        );
        let body = feed(&["v5.1.0-beta.1", "v5.0.0"]);
        let report = update_report("5.0.0-beta.1", Some(body.as_str()));
        assert!(
            report.contains("update available: 5.0.0-beta.1 → 5.1.0-beta.1"),
            "{report}"
        );
    }
}
