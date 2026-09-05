//! `donsetch -u` / `donsetch --update` : self-update from GitHub Releases.
//!
//! **No GitHub API** : uses the public releases.atom RSS feed for
//! version detection (served as a regular web page, no rate limits)
//! and direct release-asset URLs for the download. This keeps the
//! update path rate-limit-free even for anonymous, unauthenticated
//! use.
//!
//! Flow:
//!   1. Fetch `releases.atom`, parse the first `<entry><title>` tag.
//!   2. Compare with the current version (semver).
//!   3. Download the platform-correct tarball + SHA256 from
//!      `releases/download/v<tag>/donsetch-{platform}.tar.gz`.
//!   4. Verify SHA256.
//!   5. Extract (flate2 + tar).
//!   6. Replace the binary in place (atomic on Unix, rename-then-
//!      write on Windows). Also replaces pdfium.dll on Windows.
//!   7. Clean up temp files and old backups.

use std::path::Path;

use crate::DISPLAY_NAME;
use crate::cli;
use crate::fetch::client::Fetcher;
use crate::paths;
use crate::profile::BrowserProfile;

const REPO: &str = "dondai44423/donsetch";

pub async fn run() {
    cli::init();
    cli::print_title(&format!("{DISPLAY_NAME} Update"));

    let current = env!("CARGO_PKG_VERSION");
    cli::print_kv("current", current);

    // ── Binary path ──────────────────────────────────────────

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            println!("\n  {} Cannot determine binary path: {e}", cli::icon_fail());
            std::process::exit(1);
        }
    };

    // Clean up leftovers from a previous interrupted update.
    cleanup_previous(&exe);

    // ── Fetcher ──────────────────────────────────────────────

    let fetcher = match Fetcher::new(BrowserProfile::host_default()) {
        Ok(f) => f,
        Err(e) => {
            println!("\n  {} Fetcher init failed: {e}", cli::icon_fail());
            std::process::exit(1);
        }
    };

    // ── Latest version (atom feed : no API, no rate limits) ──

    let spinner = cli::Spinner::new("checking for updates...");
    let latest = match fetch_latest_version(&fetcher).await {
        Ok(v) => v,
        Err(e) => {
            spinner.stop();
            println!("  {} Could not check for updates: {e}", cli::icon_fail());
            println!("    Check your network connection and try again.");
            std::process::exit(1);
        }
    };
    spinner.stop();
    cli::print_kv("latest", &latest);
    println!();

    // ── Version comparison ───────────────────────────────────

    let cur_ver = semver::Version::parse(current).ok();
    let lat_ver = semver::Version::parse(&latest).ok();

    match (cur_ver, lat_ver) {
        (Some(c), Some(l)) if c == l => {
            println!("  Already up to date.");
            return;
        }
        (Some(c), Some(l)) if c > l => {
            println!(
                "  {} You are ahead of the latest release ({c} > {l}).",
                cli::icon_warn(),
            );
            println!("  No update needed.");
            return;
        }
        _ => {} // Proceed : version is newer or unparseable.
    }

    // ── Platform asset ───────────────────────────────────────

    let asset = match platform_asset_name() {
        Some(a) => a,
        None => {
            println!(
                "  {} Unsupported platform: {} {}",
                cli::icon_fail(),
                std::env::consts::OS,
                std::env::consts::ARCH,
            );
            std::process::exit(1);
        }
    };

    let tag = format!("v{latest}");
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let tarball_url = format!("{base}/donsetch-{asset}.tar.gz");
    let sha256_url = format!("{base}/donsetch-{asset}.tar.gz.sha256");

    let asset_label = format!("donsetch-{asset}.tar.gz");
    cli::print_kv("asset", &asset_label);
    println!();

    // ── Download tarball ─────────────────────────────────────

    let spinner = cli::Spinner::new(&format!("downloading {asset_label}"));
    let tarball = match fetcher.fetch(&tarball_url).await {
        Ok(out) if out.status == 200 => out.body,
        Ok(out) => {
            spinner.stop();
            println!(
                "  {} Download failed: HTTP {}",
                cli::icon_fail(),
                out.status
            );
            if out.status == 404 {
                println!(
                    "    No prebuilt binary for {} {}.",
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                );
                println!("    Build from source: cargo install --path .");
            }
            std::process::exit(1);
        }
        Err(e) => {
            spinner.stop();
            println!("  {} Download failed: {e}", cli::icon_fail());
            std::process::exit(1);
        }
    };
    spinner.stop();
    let mb = tarball.len() / 1_000_000;
    let kb = tarball.len() / 1_000;
    if mb > 0 {
        println!("  {} downloaded ({mb}MB)", cli::icon_pass());
    } else {
        println!("  {} downloaded ({kb}KB)", cli::icon_pass());
    }

    // ── Download + verify SHA256 ─────────────────────────────

    let sha256_text = match fetcher.fetch(&sha256_url).await {
        Ok(out) if out.status == 200 => String::from_utf8_lossy(&out.body).to_string(),
        Ok(out) => {
            println!(
                "  {} Could not download SHA256: HTTP {}",
                cli::icon_fail(),
                out.status,
            );
            std::process::exit(1);
        }
        Err(e) => {
            println!("  {} Could not download SHA256: {e}", cli::icon_fail());
            std::process::exit(1);
        }
    };

    let expected = sha256_text.split_whitespace().next().unwrap_or("");
    let actual = sha256_hex(&tarball);

    if expected.is_empty() || expected != actual {
        println!("  {} SHA256 mismatch", cli::icon_fail());
        if !expected.is_empty() {
            println!("    expected: {expected}");
            println!("    actual:   {actual}");
        }
        std::process::exit(1);
    }
    println!("  {} SHA256 verified", cli::icon_pass());

    // ── Extract ──────────────────────────────────────────────

    let temp_dir = paths::cache_dir().join("update-tmp");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).ok();

    let files = match extract_tarball(&tarball, &temp_dir) {
        Ok(f) => f,
        Err(e) => {
            println!("  {} Extraction failed: {e}", cli::icon_fail());
            let _ = std::fs::remove_dir_all(&temp_dir);
            std::process::exit(1);
        }
    };
    for f in &files {
        println!("  {} extracted {f}", cli::icon_pass());
    }

    // ── Replace binary ───────────────────────────────────────

    match replace_binary(&exe, &temp_dir) {
        Ok(()) => println!("  {} updated in place", cli::icon_pass()),
        Err(e) => {
            println!("  {} Binary replacement failed: {e}", cli::icon_fail());
            if e.contains("Permission")
                || e.contains("denied")
                || e.contains("access")
                || e.contains("read-only")
            {
                #[cfg(unix)]
                println!("    Try: sudo donsetch -u");
                #[cfg(windows)]
                println!("    Try running as administrator");
            }
            let _ = std::fs::remove_dir_all(&temp_dir);
            std::process::exit(1);
        }
    }

    // ── Clean up ─────────────────────────────────────────────

    let _ = std::fs::remove_dir_all(&temp_dir);
    println!();
    cli::print_footer();
    println!("  Updated {current} -> {latest}");
}

// ── Helpers ───────────────────────────────────────────────────

/// Map (OS, ARCH) to the release asset name suffix.
fn platform_asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("windows", "x86_64") => Some("win32-x64"),
        ("windows", "aarch64") => Some("win32-arm64"),
        _ => None,
    }
}

/// Fetch the releases.atom feed and parse the latest release tag.
///
/// The atom feed is a regular GitHub web page (not an API call),
/// so it is NOT subject to the 60-req/hour API rate limit.
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

    // Find the first <entry> block, then the <id> within it.
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

    // Strip 'v' prefix (v0.5.0-beta.1 -> 0.5.0-beta.1).
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Ok(version.to_string())
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decompress gzip and extract the tar archive into `dest`.
fn extract_tarball(data: &[u8], dest: &Path) -> Result<Vec<String>, String> {
    let tar = flate2::read::GzDecoder::new(std::io::Cursor::new(data));
    let mut archive = tar::Archive::new(tar);
    let mut files = Vec::new();

    for entry in archive.entries().map_err(|e| format!("entries: {e}"))? {
        let mut entry = entry.map_err(|e| format!("entry: {e}"))?;
        let path = entry.path().unwrap_or_default().display().to_string();
        entry
            .unpack_in(dest)
            .map_err(|e| format!("unpack {path}: {e}"))?;
        files.push(path);
    }

    Ok(files)
}

/// Replace the running binary with the extracted one.
///
/// **Unix**: copy the new binary to a temp file in the same
/// directory, then `rename()` : an atomic replace. The running
/// process keeps the old inode open. A backup is saved as
/// `donsetch.bak`.
///
/// **Windows**: the running `.exe` is locked against deletion but
/// CAN be renamed. Rename to `.exe.bak`, write the new `.exe`.
/// Also replaces `pdfium.dll` if the tarball includes it. Old
/// `.bak` files are cleaned up on the next update (see
/// `cleanup_previous`).
#[allow(clippy::needless_borrows_for_generic_args)]
fn replace_binary(exe: &Path, temp_dir: &Path) -> Result<(), String> {
    let binary_name = if cfg!(windows) {
        "donsetch.exe"
    } else {
        "donsetch"
    };
    let new_binary = temp_dir.join(binary_name);

    if !new_binary.exists() {
        return Err(format!(
            "extracted binary not found: {}",
            new_binary.display()
        ));
    }

    let exe_dir = exe
        .parent()
        .ok_or_else(|| "cannot determine binary directory".to_string())?;

    // Borrow as &Path for fs operations : &Path is Copy, so it
    // won't move and won't trigger clippy::needless_borrows.

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // Copy new binary to a temp file in the same dir (atomic rename target).
        let tmp = exe_dir.join(".donsetch.update.tmp");
        std::fs::copy(&new_binary, &tmp).map_err(|e| format!("copy: {e}"))?;

        // Set executable permission.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod: {e}"))?;

        // Save backup (copy, not rename : keeps the original in place).
        let bak = exe_dir.join("donsetch.bak");
        if let Err(e) = std::fs::copy(&exe, &bak) {
            // The atomic replace below is still checked; but if the
            // backup copy failed, rollback will be impossible after
            // the swap : the user must know BEFORE it happens.
            println!(
                "  {} Warning: backup copy failed ({e}) : rollback will not be possible for this update",
                cli::icon_warn()
            );
        }
        // Write version metadata for rollback.
        let _ = std::fs::write(exe_dir.join("donsetch.bak.ver"), env!("CARGO_PKG_VERSION"));

        // Atomic replace.
        std::fs::rename(&tmp, &exe).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("rename: {e}")
        })?;
    }

    #[cfg(windows)]
    {
        // Rename running .exe to .bak (Windows allows renaming a running exe).
        let bak = exe.with_extension("exe.bak");
        let _ = std::fs::remove_file(&bak); // Remove old .bak from previous update.

        std::fs::rename(&exe, &bak).map_err(|e| format!("rename old: {e}"))?;

        std::fs::copy(&new_binary, &exe).map_err(|e| {
            // Restore from backup on failure.
            let _ = std::fs::rename(&bak, &exe);
            format!("copy new: {e}")
        })?;

        // Write version metadata for rollback.
        let _ = std::fs::write(exe_dir.join("donsetch.bak.ver"), env!("CARGO_PKG_VERSION"));

        // Copy pdfium.dll if present in the tarball.
        let new_dll = temp_dir.join("pdfium.dll");
        if new_dll.exists() {
            let dll_path = exe_dir.join("pdfium.dll");
            let dll_bak = exe_dir.join("pdfium.dll.bak");
            let _ = std::fs::remove_file(&dll_bak);
            let _ = std::fs::rename(&dll_path, &dll_bak);
            let _ = std::fs::copy(&new_dll, &dll_path);
        }
    }

    Ok(())
}

/// Remove temp files from a previous interrupted update.
/// Does NOT remove .bak files : those are managed by replace_binary
/// and needed for rollback. Only cleans up temp artifacts.
fn cleanup_previous(exe: &Path) {
    let exe_dir = exe.parent().unwrap_or_else(|| Path::new("."));

    // Temp download dir.
    let temp_dir = paths::cache_dir().join("update-tmp");
    let _ = std::fs::remove_dir_all(&temp_dir);

    // Unix temp file (half-written binary from interrupted update).
    let tmp = exe_dir.join(".donsetch.update.tmp");
    let _ = std::fs::remove_file(&tmp);
}
