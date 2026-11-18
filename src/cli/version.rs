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

pub fn help() {
    cli::init();
    cli::print_header(
        &format!("{DISPLAY_NAME} Version"),
        "build identity & release check",
    );
    println!();
    println!("  {} {}", cli::dim("Usage:"), cli::bold("donsetch version"));
    println!(
        "         {}",
        cli::dim("aliases: donsetch -V  ·  donsetch -v  ·  donsetch --version")
    );
    println!();
    println!(
        "  {}",
        cli::dim(
            "Prints binary path, target triple, active wire profile, compiled features,\n  PDFium build, git revision, and live release status (skip with DONSETCH_NO_UPDATE_CHECK=1)."
        )
    );
    println!();
    cli::print_nav(&[
        ("donsetch update", "upgrade binary"),
        ("donsetch status", "runtime overview"),
    ]);
}

pub async fn run(args: &[String]) {
    crate::cli::args::Spec::new("version", "donsetch version", &[]).reject(&args[2..]);
    crate::cli::init();

    let current = env!("CARGO_PKG_VERSION");
    let target = option_env!("DONSETCH_TARGET").unwrap_or("local");
    cli::print_header(&format!("{DISPLAY_NAME} {current}"), target);
    println!();

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    cli::print_kv("binary", &cli::dim(&exe));
    cli::print_kv("target", target);
    let profile = BrowserProfile::host_default();
    let label = match crate::profile::probe_installed() {
        Some(_) => format!("{} {}", cli::cyan(&profile.name), cli::dim("(detected)")),
        None => format!(
            "{} {}",
            cli::cyan(&profile.name),
            cli::dim("(fallback: no installed browser detected)")
        ),
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

    let skip_update_check =
        std::env::var_os("DONSETCH_NO_UPDATE_CHECK").is_some() || std::env::var_os("CI").is_some();
    let feed = if skip_update_check {
        None
    } else {
        let spin = cli::Spinner::new_tty_only("Checking latest release\u{2026}");
        let fetcher = Fetcher::new(BrowserProfile::host_default());
        let fetched = match fetcher {
            Ok(f) => tokio::time::timeout(Duration::from_secs(3), update::fetch_feed(&f))
                .await
                .ok()
                .and_then(Result::ok),
            Err(_) => None,
        };
        spin.stop();
        fetched
    };

    println!();

    if skip_update_check {
        println!("  {}", cli::dim("(update check skipped)"));
        return;
    }

    println!("{}", update_report(current, feed.as_deref()));
    println!();
    cli::print_footer();
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
