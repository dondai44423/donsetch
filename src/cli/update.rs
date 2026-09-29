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
//!      write on Windows). Also replaces pdfium.dll and onnxruntime.dll on Windows.
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
        // No Windows ARM64 asset is built; run the x64 binary under emulation.
        ("windows", "aarch64") => Some("win32-x64"),
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
/// Also replaces `pdfium.dll` and `onnxruntime.dll` if the tarball includes them. Old
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

        // Stage the sibling runtime libs the tarball ships beside
        // the binary (Linux: libonnxruntime.so, which onnx.rs dlopens
        // from the exe dir first) BEFORE anything is renamed, so the
        // one realistic failure here -- disk full on a 22MB copy --
        // leaves the install untouched. Swapping only the binary left
        // the old runtime next to the new build: the .so that needed
        // GLIBC_2.38 stayed put through `-u` after the release that
        // replaced it, so OCR/rerank stayed dead for self-updaters
        // while `doctor` (a presence check) reported it fine. The
        // Windows branch below already does this for pdfium.dll.
        let mut staged_libs: Vec<&str> = Vec::new();
        for name in SIBLING_LIBS {
            let new_lib = temp_dir.join(name);
            if !new_lib.exists() {
                continue;
            }
            let lib_tmp = sibling_tmp(exe_dir, name);
            if let Err(e) = std::fs::copy(&new_lib, &lib_tmp) {
                let _ = std::fs::remove_file(&tmp);
                for staged in &staged_libs {
                    let _ = std::fs::remove_file(sibling_tmp(exe_dir, staged));
                }
                let _ = std::fs::remove_file(&lib_tmp);
                return Err(format!("{name}: copy: {e}"));
            }
            staged_libs.push(name);
        }

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
            for staged in &staged_libs {
                let _ = std::fs::remove_file(sibling_tmp(exe_dir, staged));
            }
            format!("rename: {e}")
        })?;

        // Swap the staged libs in, same shape as the binary: back up
        // by COPY so a live lib exists at every instant, then one
        // atomic rename. A stale .bak from an earlier update is
        // cleared first so rollback never pairs binary N with lib
        // N-1. The binary is already updated at this point, so a
        // failure here is a warning naming the stale file, not an
        // error that would make `-u` report "already up to date"
        // with no way to refresh the lib.
        for name in staged_libs {
            let lib = exe_dir.join(name);
            let lib_bak = exe_dir.join(format!("{name}.bak"));
            let lib_tmp = sibling_tmp(exe_dir, name);
            let _ = std::fs::remove_file(&lib_bak);
            if lib.exists()
                && let Err(e) = std::fs::copy(&lib, &lib_bak)
            {
                println!(
                    "  {} Warning: could not back up {name} ({e}) : rollback will keep the new one",
                    cli::icon_warn()
                );
            }
            if let Err(e) = std::fs::rename(&lib_tmp, &lib) {
                let _ = std::fs::remove_file(&lib_tmp);
                println!(
                    "  {} Warning: could not replace {name} ({e}) : the previous one is still in use",
                    cli::icon_warn()
                );
            }
        }
    }

    #[cfg(windows)]
    {
        // Stage the sibling DLLs the tarball ships beside the exe
        // BEFORE anything is renamed, so the one realistic failure
        // here -- disk full on a ~20MB copy -- leaves the install
        // untouched. The old rename-then-copy loop ignored every
        // error (`let _ =`): a failed copy left ZERO pdfium.dll next
        // to a freshly swapped exe while `-u` printed "updated in
        // place".
        let mut staged_libs: Vec<&str> = Vec::new();
        for name in SIBLING_LIBS {
            if !temp_dir.join(name).exists() {
                continue;
            }
            if let Err(e) = stage_sibling_dll(exe_dir, temp_dir, name) {
                for staged in &staged_libs {
                    let _ = std::fs::remove_file(sibling_tmp(exe_dir, staged));
                }
                return Err(e);
            }
            staged_libs.push(name);
        }

        // Rename running .exe to .bak (Windows allows renaming a running exe).
        let bak = exe.with_extension("exe.bak");
        let _ = std::fs::remove_file(&bak); // Remove old .bak from previous update.

        std::fs::rename(&exe, &bak).map_err(|e| {
            for staged in &staged_libs {
                let _ = std::fs::remove_file(sibling_tmp(exe_dir, staged));
            }
            format!("rename old: {e}")
        })?;

        std::fs::copy(&new_binary, &exe).map_err(|e| {
            // Restore from backup on failure.
            let _ = std::fs::rename(&bak, &exe);
            for staged in &staged_libs {
                let _ = std::fs::remove_file(sibling_tmp(exe_dir, staged));
            }
            format!("copy new: {e}")
        })?;

        // Write version metadata for rollback.
        let _ = std::fs::write(exe_dir.join("donsetch.bak.ver"), env!("CARGO_PKG_VERSION"));

        // Land the staged DLLs. A loaded DLL on Windows may be
        // renamed but not written or deleted, so the live one moves
        // aside to `<name>.bak` and the staged file takes its place.
        // The binary is already updated, so a failure here is a
        // warning naming the file -- same contract as the Unix
        // sibling swap above -- never a silent `Ok`.
        for name in staged_libs {
            if let Err(e) = land_sibling_dll(exe_dir, name) {
                println!(
                    "  {} Warning: could not replace {name} ({e})",
                    cli::icon_warn()
                );
            }
        }
    }

    Ok(())
}

/// Runtime libraries a release tarball may ship beside the binary
/// and the updater must carry across upgrades: Linux
/// `libonnxruntime.so`, macOS `libonnxruntime.dylib` since #316
/// (both dlopen'd from the exe dir by onnx.rs), Windows
/// `pdfium.dll` and `onnxruntime.dll`. A tarball without one simply
/// stages nothing. Rollback restores the same list so a rolled-back
/// binary never keeps a newer sibling runtime.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) const SIBLING_LIBS: &[&str] = &["libonnxruntime.so"];

#[cfg(all(unix, target_os = "macos"))]
pub(crate) const SIBLING_LIBS: &[&str] = &["libonnxruntime.dylib"];

#[cfg(windows)]
pub(crate) const SIBLING_LIBS: &[&str] = &["pdfium.dll", "onnxruntime.dll"];

fn sibling_tmp(exe_dir: &Path, name: &str) -> std::path::PathBuf {
    exe_dir.join(format!(".{name}.update.tmp"))
}

/// Stage one sibling runtime from the extract dir beside the exe as
/// `.{name}.update.tmp`. Nothing live is touched: a failure here
/// (disk full, unreadable source) leaves the install exactly as it
/// was. Compiled for the Windows swap and for its unit tests.
#[cfg(any(windows, test))]
fn stage_sibling_dll(exe_dir: &Path, temp_dir: &Path, name: &str) -> Result<(), String> {
    let new_dll = temp_dir.join(name);
    let dll_tmp = sibling_tmp(exe_dir, name);
    if let Err(e) = std::fs::copy(&new_dll, &dll_tmp) {
        let _ = std::fs::remove_file(&dll_tmp);
        return Err(format!("{name}: copy: {e}"));
    }
    Ok(())
}

/// Land a staged sibling runtime: the live file moves aside to
/// `<name>.bak` (a loaded DLL on Windows can be renamed but not
/// written or deleted, so backup-by-rename is the only sequence that
/// works there) and the staged file takes its place. On failure the
/// previous file is put back, so the exe dir never ends up with zero
/// copies of a runtime the binary needs.
#[cfg(any(windows, test))]
fn land_sibling_dll(exe_dir: &Path, name: &str) -> Result<(), String> {
    let dll = exe_dir.join(name);
    let dll_bak = exe_dir.join(format!("{name}.bak"));
    let dll_tmp = sibling_tmp(exe_dir, name);
    let _ = std::fs::remove_file(&dll_bak);
    let had_live = dll.exists();
    if had_live {
        std::fs::rename(&dll, &dll_bak).map_err(|e| format!("{name}: move current aside: {e}"))?;
    }
    if let Err(e) = std::fs::rename(&dll_tmp, &dll) {
        let _ = std::fs::remove_file(&dll_tmp);
        if had_live {
            let _ = std::fs::rename(&dll_bak, &dll);
        }
        return Err(format!("{name}: install new: {e}"));
    }
    Ok(())
}

/// Rollback's counterpart to the sibling-lib refresh in
/// `replace_binary`: swap `name` and `name.bak` so the previous
/// binary gets its previous runtime back. No `.bak` means the last
/// update shipped no lib (or predates this): nothing to do.
///
/// The current lib is stashed by hard link (copy if the filesystem
/// refuses), so `name` exists at every instant and a crash midway
/// costs at most the roll-forward copy.
pub(crate) fn swap_sibling_lib(exe_dir: &Path, name: &str) -> Result<(), String> {
    let cur = exe_dir.join(name);
    let bak = exe_dir.join(format!("{name}.bak"));
    if !bak.exists() {
        return Ok(());
    }
    let stash = exe_dir.join(format!(".{name}.rollback.tmp"));
    let _ = std::fs::remove_file(&stash);
    let have_cur = cur.exists();
    if have_cur
        && std::fs::hard_link(&cur, &stash).is_err()
        && let Err(e) = std::fs::copy(&cur, &stash)
    {
        return Err(format!("{name}: stash current: {e}"));
    }
    if let Err(e) = std::fs::rename(&bak, &cur) {
        let _ = std::fs::remove_file(&stash);
        return Err(format!("{name}: restore backup: {e}"));
    }
    if have_cur && let Err(e) = std::fs::rename(&stash, &bak) {
        let _ = std::fs::remove_file(&stash);
        return Err(format!("{name}: keep roll-forward: {e}"));
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

    // Staging temps from an interrupted update (half-written binary
    // / lib / DLL).
    let tmp = exe_dir.join(".donsetch.update.tmp");
    let _ = std::fs::remove_file(&tmp);
    for name in SIBLING_LIBS {
        let _ = std::fs::remove_file(sibling_tmp(exe_dir, name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("donsetch-update-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    fn bin_name() -> &'static str {
        if cfg!(windows) {
            "donsetch.exe"
        } else {
            "donsetch"
        }
    }

    // A release tarball ships the runtime sibling beside the binary
    // (Linux .so, macOS .dylib, Windows pdfium.dll + onnxruntime.dll),
    // and onnx.rs loads it from the exe dir first. An update that
    // swaps only the binary leaves a stale runtime next to the new
    // build.
    #[test]
    fn replace_binary_refreshes_sibling_runtime_lib() {
        let root = scratch("replace");
        let exe_dir = root.join("bin");
        let temp_dir = root.join("update-tmp");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        let exe = exe_dir.join(bin_name());
        std::fs::write(&exe, "old-bin").unwrap();
        std::fs::write(temp_dir.join(bin_name()), "new-bin").unwrap();
        for name in SIBLING_LIBS {
            std::fs::write(exe_dir.join(name), format!("old-{name}")).unwrap();
            std::fs::write(temp_dir.join(name), format!("new-{name}")).unwrap();
        }

        replace_binary(&exe, &temp_dir).expect("replace");

        assert_eq!(read(&exe), "new-bin");
        assert_eq!(
            read(&exe_dir.join(format!("{}.bak", bin_name()))),
            "old-bin"
        );
        for name in SIBLING_LIBS {
            assert_eq!(
                read(&exe_dir.join(name)),
                format!("new-{name}"),
                "sibling runtime lib not refreshed: {name}"
            );
            assert_eq!(
                read(&exe_dir.join(format!("{name}.bak"))),
                format!("old-{name}"),
                "previous runtime lib not kept for rollback: {name}"
            );
        }
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
                assert!(
                    mode & 0o111 != 0,
                    "binary lost its executable bit: {mode:o}"
                );
            }
        }
        #[cfg(unix)]
        for name in SIBLING_LIBS {
            assert!(
                !sibling_tmp(&exe_dir, name).exists(),
                "staging tmp left behind: {name}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // First install had no lib beside the binary (older release, or
    // the lib lived only in the cache dir): it appears, with no .bak.
    #[test]
    fn replace_binary_installs_lib_that_was_not_there_before() {
        let root = scratch("firstlib");
        let exe_dir = root.join("bin");
        let temp_dir = root.join("update-tmp");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        let exe = exe_dir.join(bin_name());
        std::fs::write(&exe, "old-bin").unwrap();
        std::fs::write(temp_dir.join(bin_name()), "new-bin").unwrap();
        for name in SIBLING_LIBS {
            std::fs::write(temp_dir.join(name), format!("new-{name}")).unwrap();
        }

        replace_binary(&exe, &temp_dir).expect("replace");

        for name in SIBLING_LIBS {
            assert_eq!(read(&exe_dir.join(name)), format!("new-{name}"), "{name}");
            assert!(
                !exe_dir.join(format!("{name}.bak")).exists(),
                "{name}.bak must not appear when there was no prior lib"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A .bak left by an earlier update must not survive: rollback
    // would otherwise pair binary N with lib N-1.
    #[test]
    fn replace_binary_replaces_a_stale_lib_backup() {
        let root = scratch("stalebak");
        let exe_dir = root.join("bin");
        let temp_dir = root.join("update-tmp");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        let exe = exe_dir.join(bin_name());
        std::fs::write(&exe, "bin-2").unwrap();
        std::fs::write(temp_dir.join(bin_name()), "bin-3").unwrap();
        for name in SIBLING_LIBS {
            std::fs::write(exe_dir.join(name), format!("so-2-{name}")).unwrap();
            std::fs::write(exe_dir.join(format!("{name}.bak")), format!("so-1-{name}")).unwrap();
            std::fs::write(temp_dir.join(name), format!("so-3-{name}")).unwrap();
        }

        replace_binary(&exe, &temp_dir).expect("replace");

        for name in SIBLING_LIBS {
            assert_eq!(read(&exe_dir.join(name)), format!("so-3-{name}"), "{name}");
            assert_eq!(
                read(&exe_dir.join(format!("{name}.bak"))),
                format!("so-2-{name}"),
                "{name}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A tarball without the lib (macOS before #316, or a future
    // static build) must leave whatever is beside the binary alone.
    #[test]
    fn replace_binary_leaves_lib_alone_when_tarball_has_none() {
        let root = scratch("nolib");
        let exe_dir = root.join("bin");
        let temp_dir = root.join("update-tmp");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        let exe = exe_dir.join(bin_name());
        std::fs::write(&exe, "old-bin").unwrap();
        std::fs::write(temp_dir.join(bin_name()), "new-bin").unwrap();
        for name in SIBLING_LIBS {
            std::fs::write(exe_dir.join(name), format!("old-{name}")).unwrap();
        }

        replace_binary(&exe, &temp_dir).expect("replace");

        assert_eq!(read(&exe), "new-bin");
        for name in SIBLING_LIBS {
            assert_eq!(read(&exe_dir.join(name)), format!("old-{name}"), "{name}");
            assert!(
                !exe_dir.join(format!("{name}.bak")).exists(),
                "{name}.bak must not appear when the tarball shipped no lib"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // Rollback's counterpart: put the previous lib back beside the
    // previous binary. Every name the updater stages has to come
    // back, not just the first one (Windows rolls back pdfium.dll
    // and onnxruntime.dll together).
    #[test]
    fn swap_sibling_lib_round_trips_every_staged_runtime() {
        let root = scratch("swap");
        for name in SIBLING_LIBS {
            std::fs::write(root.join(name), format!("new-{name}")).unwrap();
            std::fs::write(root.join(format!("{name}.bak")), format!("old-{name}")).unwrap();
        }

        for name in SIBLING_LIBS {
            swap_sibling_lib(&root, name).expect("swap");
            assert_eq!(read(&root.join(name)), format!("old-{name}"), "{name}");
            assert_eq!(
                read(&root.join(format!("{name}.bak"))),
                format!("new-{name}"),
                "{name}"
            );
        }

        // No .bak: nothing to do, not an error.
        for name in SIBLING_LIBS {
            std::fs::remove_file(root.join(format!("{name}.bak"))).unwrap();
        }
        for name in SIBLING_LIBS {
            swap_sibling_lib(&root, name).expect("no-op");
            assert_eq!(read(&root.join(name)), format!("old-{name}"), "{name}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A rolled-back binary must not keep a newer sibling runtime:
    // update stages every SIBLING_LIBS name, rollback restores every
    // one. This is the Windows asymmetry the 4.3.7 self-audit left.
    #[test]
    fn rollback_restores_every_sibling_lib_the_updater_staged() {
        let root = scratch("rollback-libs");
        let exe_dir = root.join("bin");
        let temp_dir = root.join("update-tmp");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::write(exe_dir.join(bin_name()), "old-bin").unwrap();
        std::fs::write(temp_dir.join(bin_name()), "new-bin").unwrap();
        for name in SIBLING_LIBS {
            std::fs::write(exe_dir.join(name), format!("old-{name}")).unwrap();
            std::fs::write(temp_dir.join(name), format!("new-{name}")).unwrap();
        }

        replace_binary(&exe_dir.join(bin_name()), &temp_dir).expect("replace");

        for name in SIBLING_LIBS {
            assert_eq!(read(&exe_dir.join(name)), format!("new-{name}"), "{name}");
            assert_eq!(
                read(&exe_dir.join(format!("{name}.bak"))),
                format!("old-{name}"),
                "{name}"
            );
        }

        for name in SIBLING_LIBS {
            swap_sibling_lib(&exe_dir, name).expect("rollback lib");
            assert_eq!(
                read(&exe_dir.join(name)),
                format!("old-{name}"),
                "{name} after rollback"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // The Windows DLL swap used to rename the live dll aside and then
    // `let _ =` the copy of the new one: a failed copy (disk full, AV
    // lock) left ZERO dlls beside the freshly swapped exe while `-u`
    // still printed "updated in place". Staging must be all-or-
    // nothing BEFORE anything live is touched.
    #[test]
    fn sibling_dll_staging_failure_leaves_the_install_untouched() {
        let root = scratch("dll-stagefail");
        let exe_dir = root.join("bin");
        let temp_dir = root.join("update-tmp");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        let name = "pdfium.dll";
        std::fs::write(exe_dir.join(name), "old-dll").unwrap();
        std::fs::write(exe_dir.join(format!("{name}.bak")), "older-dll").unwrap();
        // A directory where the source file should be: copy fails.
        std::fs::create_dir_all(temp_dir.join(name)).unwrap();

        stage_sibling_dll(&exe_dir, &temp_dir, name).expect_err("staging must fail");
        assert_eq!(read(&exe_dir.join(name)), "old-dll", "live dll was touched");
        assert_eq!(
            read(&exe_dir.join(format!("{name}.bak"))),
            "older-dll",
            "backup was touched"
        );
        assert!(
            !sibling_tmp(&exe_dir, name).exists(),
            "half-written staging tmp left behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Landing must keep a live dll at every instant and put the old
    // one back if the staged file cannot take its place: the exe dir
    // must never end up with zero copies of a runtime the binary
    // needs.
    #[test]
    fn sibling_dll_land_restores_the_previous_dll_when_the_install_fails() {
        let root = scratch("dll-landfail");
        let exe_dir = root.join("bin");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let name = "onnxruntime.dll";
        std::fs::write(exe_dir.join(name), "old-dll").unwrap();
        // No staged file (an interrupted update): the install rename
        // fails and the old dll must come back, not sit in .bak with
        // nothing at `name`.
        land_sibling_dll(&exe_dir, name).expect_err("install must fail");
        assert_eq!(
            read(&exe_dir.join(name)),
            "old-dll",
            "old dll was not restored"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sibling_dll_stage_land_swaps_and_keeps_the_backup() {
        let root = scratch("dll-swap");
        let exe_dir = root.join("bin");
        let temp_dir = root.join("update-tmp");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        let name = "pdfium.dll";
        std::fs::write(exe_dir.join(name), "old-dll").unwrap();
        std::fs::write(temp_dir.join(name), "new-dll").unwrap();

        stage_sibling_dll(&exe_dir, &temp_dir, name).expect("stage");
        land_sibling_dll(&exe_dir, name).expect("land");

        assert_eq!(read(&exe_dir.join(name)), "new-dll");
        assert_eq!(read(&exe_dir.join(format!("{name}.bak"))), "old-dll");
        assert!(!sibling_tmp(&exe_dir, name).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
