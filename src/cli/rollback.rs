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
        use std::os::unix::fs::PermissionsExt;

        // Strategy: copy .bak to a temp, then atomic rename over exe.
        // The old exe becomes the new .bak (for roll-forward).
        let tmp = exe_dir.join(".donsetch.rollback.tmp");

        // Copy backup to temp.
        if let Err(e) = std::fs::copy(&bak_path, &tmp).map_err(|e| e.to_string()) {
            println!("  {} Copy backup failed: {e}", cli::icon_fail());
            std::process::exit(1);
        }

        // Set executable.
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())
        {
            let _ = std::fs::remove_file(&tmp);
            println!("  {} chmod failed: {e}", cli::icon_fail());
            std::process::exit(1);
        }

        // Save current as new backup (for roll-forward).
        if let Err(e) = std::fs::copy(&exe, &bak_path).map_err(|e| e.to_string()) {
            let _ = std::fs::remove_file(&tmp);
            println!("  {} Save current as backup failed: {e}", cli::icon_fail());
            std::process::exit(1);
        }

        // Write new backup version (the version we just rolled away from).
        let _ = std::fs::write(&bak_ver_path, current);

        // Atomic replace.
        if let Err(e) = std::fs::rename(&tmp, &exe).map_err(|e| e.to_string()) {
            let _ = std::fs::remove_file(&tmp);
            println!("  {} Atomic rename failed: {e}", cli::icon_fail());
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
            return;
        }

        // Rename old current to .bak (new backup for roll-forward).
        if let Err(e) = std::fs::rename(&tmp, &bak_path).map_err(|e| e.to_string()) {
            // Not fatal : the rollback succeeded, we just couldn't
            // save the roll-forward backup.
            println!(
                "  {} Rollback OK, but could not save roll-forward backup: {e}",
                cli::icon_warn()
            );
        }

        // Write new backup version.
        let _ = std::fs::write(&bak_ver_path, current);

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
