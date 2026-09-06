//! `donsetch --rollback` : revert to the previous binary version.
//!
//! Swaps the current binary with the `.bak` backup saved by
//! `--update`. The current binary becomes the new `.bak` (so
//! `--rollback` again rolls forward). Version metadata in
//! `donsetch.bak.ver` tracks which version is in the backup.
//!
//! Cross-platform:
//!   Unix:   atomic rename swap. The running process keeps its
//!           inode open.
//!   Windows: rename running .exe to .bak, copy old .bak to .exe.
//!           Also swaps pdfium.dll if a .dll.bak exists.

use std::path::{Path, PathBuf};

use crate::DISPLAY_NAME;
use crate::cli;

#[allow(clippy::needless_borrows_for_generic_args)]
pub fn run() {
    cli::init();
    cli::print_title(&format!("{DISPLAY_NAME} Rollback"));

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

    let exe_dir = exe.parent().unwrap_or_else(|| Path::new("."));

    // Borrow as &Path for fs operations : &Path is Copy, so it
    // won't move and won't trigger clippy::needless_borrows.

    // ── Locate backup ────────────────────────────────────────

    #[cfg(unix)]
    let (bak_path, bak_ver_path): (PathBuf, PathBuf) = {
        (
            exe_dir.join("donsetch.bak"),
            exe_dir.join("donsetch.bak.ver"),
        )
    };

    #[cfg(windows)]
    let (bak_path, bak_ver_path): (PathBuf, PathBuf) = {
        (
            exe.with_extension("exe.bak"),
            exe_dir.join("donsetch.bak.ver"),
        )
    };

    if !bak_path.exists() {
        println!();
        println!("  {} No backup found.", cli::icon_fail());
        println!("    Run `donsetch -u` to update first; a backup is saved automatically.");
        std::process::exit(1);
    }

    // ── Read backup version ──────────────────────────────────

    let bak_ver = std::fs::read_to_string(&bak_ver_path)
        .unwrap_or_default()
        .trim()
        .to_string();

    if bak_ver.is_empty() {
        cli::print_kv("backup", "unknown version");
    } else if bak_ver == current {
        println!();
        println!(
            "  {} Backup is the same version ({current}).",
            cli::icon_warn()
        );
        println!("    Nothing to roll back to.");
        std::process::exit(1);
    } else {
        cli::print_kv("backup", &bak_ver);
    }

    // ── Integrity check ──────────────────────────────────────

    let bak_meta = match std::fs::metadata(&bak_path) {
        Ok(m) => m,
        Err(e) => {
            println!("\n  {} Cannot read backup: {e}", cli::icon_fail());
            std::process::exit(1);
        }
    };

    if bak_meta.len() < 1_000_000 {
        println!(
            "\n  {} Backup appears corrupt ({} bytes).",
            cli::icon_fail(),
            bak_meta.len(),
        );
        println!("    Run `donsetch -u` to download a fresh copy.");
        std::process::exit(1);
    }

    // ── Swap ──────────────────────────────────────────────────

    println!();

    #[cfg(unix)]
    {
        match swap_unix(&exe, &bak_path, &bak_ver_path, current) {
            Ok(Some(warning)) => println!("  {} {warning}", cli::icon_warn()),
            Ok(None) => {}
            Err(e) => {
                println!("  {} {e}", cli::icon_fail());
                if e.contains("Permission")
                    || e.contains("denied")
                    || e.contains("access")
                    || e.contains("read-only")
                {
                    println!("    Try: sudo donsetch --rollback");
                }
                std::process::exit(1);
            }
        }

        // The previous binary gets its previous runtime lib back
        // (update keeps it as `<lib>.bak`); the current lib becomes
        // the roll-forward backup, same as the binary. Not fatal:
        // the binary rollback above already succeeded.
        for name in crate::cli::update::SIBLING_LIBS {
            if let Err(e) = crate::cli::update::swap_sibling_lib(exe_dir, name) {
                println!(
                    "  {} Rolled back the binary, but could not restore {name}: {e}",
                    cli::icon_warn()
                );
            }
        }
    }

    #[cfg(windows)]
    {
        // Windows: rename running .exe to .rollback.tmp (allowed),
        // copy .bak to .exe, then rename .rollback.tmp to .bak.
        let tmp = exe_dir.join(".donsetch.rollback.tmp");

        // Remove stale temp from interrupted rollback.
        let _ = std::fs::remove_file(&tmp);

        // Rename current running exe to temp.
        if let Err(e) = std::fs::rename(&exe, &tmp).map_err(|e| e.to_string()) {
            println!("  {} Rename current failed: {e}", cli::icon_fail());
            std::process::exit(1);
        }

        // Copy backup to exe path.
        if let Err(e) = std::fs::copy(&bak_path, &exe).map_err(|e| {
            // Restore on failure.
            let _ = std::fs::rename(&tmp, &exe);
            e.to_string()
        }) {
            println!("  {} Copy backup failed: {e}", cli::icon_fail());
            // A plain `return` here reported success (exit 0) for
            // a rollback that did not happen.
            std::process::exit(1);
        }

        // Rename old current to .bak (new backup for roll-forward).
        match std::fs::rename(&tmp, &bak_path) {
            // Write new backup version.
            Ok(()) => {
                let _ = std::fs::write(&bak_ver_path, current);
            }
            Err(e) => {
                // Not fatal : the rollback succeeded, we just couldn't
                // save the roll-forward backup. `.bak.ver` is left as
                // is: it still describes what `.bak` holds.
                println!(
                    "  {} Rollback OK, but could not save roll-forward backup: {e}",
                    cli::icon_warn()
                );
            }
        }

        // Swap pdfium.dll if backups exist.
        let dll_path = exe_dir.join("pdfium.dll");
        let dll_bak = exe_dir.join("pdfium.dll.bak");
        if dll_bak.exists() && dll_path.exists() {
            let dll_tmp = exe_dir.join(".pdfium.rollback.tmp");
            let _ = std::fs::remove_file(&dll_tmp);
            if std::fs::rename(&dll_path, &dll_tmp).is_ok() {
                if std::fs::copy(&dll_bak, &dll_path).is_ok() {
                    let _ = std::fs::rename(&dll_tmp, &dll_bak);
                } else {
                    let _ = std::fs::rename(&dll_tmp, &dll_path);
                }
            }
        }
    }

    println!("  {} rolled back", cli::icon_pass());
    if !bak_ver.is_empty() {
        println!("  {} {} -> {}", cli::icon_pass(), current, bak_ver);
    }

    println!();
    cli::print_footer();
    if !bak_ver.is_empty() {
        println!("  Rolled back {current} -> {bak_ver}");
    } else {
        println!("  Rolled back to previous version");
    }
}

/// Unix swap: the backup becomes the binary, the binary becomes the
/// backup (for roll-forward). Returns `Ok(Some(warning))` when the
/// rollback itself succeeded but the roll-forward backup could not
/// be saved.
///
/// Ordering matters: the previous version exists only in `.bak`, so
/// `.bak` must not be overwritten until the new binary is in place.
/// The old sequence copied the current binary OVER `.bak` before the
/// final rename and deleted its temp on rename failure -- a failed
/// rename (sticky-bit dir, immutable file) destroyed the only copy
/// of the previous version and left `.bak` holding the current one.
#[cfg(unix)]
fn swap_unix(
    exe: &Path,
    bak_path: &Path,
    bak_ver_path: &Path,
    current: &str,
) -> Result<Option<String>, String> {
    use std::os::unix::fs::PermissionsExt;

    let exe_dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let tmp = exe_dir.join(".donsetch.rollback.tmp");
    let bak_tmp = exe_dir.join(".donsetch.bak.rollback.tmp");
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&bak_tmp);

    // Stage: the backup as the future binary, the current binary as
    // the future backup. Nothing live is touched yet.
    std::fs::copy(bak_path, &tmp).map_err(|e| format!("Copy backup failed: {e}"))?;
    if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("chmod failed: {e}"));
    }
    if let Err(e) = std::fs::copy(exe, &bak_tmp) {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&bak_tmp);
        return Err(format!("Save current as backup failed: {e}"));
    }

    // Atomic replace. On failure both stages are discarded and the
    // original `.bak` is untouched.
    if let Err(e) = std::fs::rename(&tmp, exe) {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&bak_tmp);
        return Err(format!("Atomic rename failed: {e}"));
    }

    // Only now does the previous backup get replaced. If this fails,
    // `.bak.ver` is deliberately left alone: it still describes what
    // `.bak` holds (the version now running), so the next --rollback
    // correctly refuses with "same version" instead of swapping a
    // binary with itself.
    if let Err(e) = std::fs::rename(&bak_tmp, bak_path) {
        let _ = std::fs::remove_file(&bak_tmp);
        return Ok(Some(format!(
            "Rollback OK, but could not save roll-forward backup: {e}"
        )));
    }
    // Version metadata: the version we just rolled away from.
    let _ = std::fs::write(bak_ver_path, current);
    Ok(None)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "donsetch-rollback-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn swap_exchanges_binary_and_backup() {
        let dir = scratch("swap");
        let exe = dir.join("donsetch");
        let bak = dir.join("donsetch.bak");
        let ver = dir.join("donsetch.bak.ver");
        std::fs::write(&exe, "current").unwrap();
        std::fs::write(&bak, "previous").unwrap();

        let warn = swap_unix(&exe, &bak, &ver, "9.9.9").expect("swap");

        assert_eq!(warn, None);
        assert_eq!(read(&exe), "previous");
        assert_eq!(read(&bak), "current");
        assert_eq!(read(&ver), "9.9.9");
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "rolled-back binary not executable");
        }
        assert!(!dir.join(".donsetch.rollback.tmp").exists());
        assert!(!dir.join(".donsetch.bak.rollback.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Any failure before the swap completes must leave `.bak` (the
    // only copy of the previous version) untouched and no staging
    // files behind. A rename-over-immutable-file failure can't be
    // forced portably in a unit test; the reachable failure here is
    // the current binary being unreadable as a file (a directory in
    // its place), which trips the same invariant. The rename case
    // is covered by ordering: `.bak` is not written until after the
    // rename has succeeded.
    #[test]
    fn failure_before_the_swap_leaves_the_backup_intact() {
        let dir = scratch("stagefail");
        let exe = dir.join("donsetch");
        std::fs::create_dir_all(exe.join("occupied")).unwrap();
        let bak = dir.join("donsetch.bak");
        let ver = dir.join("donsetch.bak.ver");
        std::fs::write(&bak, "previous").unwrap();

        let err = swap_unix(&exe, &bak, &ver, "9.9.9").expect_err("must fail");
        assert!(err.contains("failed"), "unexpected error shape: {err}");
        assert_eq!(read(&bak), "previous", "backup was destroyed");
        assert!(!ver.exists());
        assert!(!dir.join(".donsetch.rollback.tmp").exists());
        assert!(!dir.join(".donsetch.bak.rollback.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
