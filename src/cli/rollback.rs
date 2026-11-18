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
//!           Also swaps pdfium.dll and onnxruntime.dll if a .bak exists.

use std::path::{Path, PathBuf};

use crate::DISPLAY_NAME;
use crate::cli;

pub fn help() {
    cli::init();
    cli::print_header(
        &format!("{DISPLAY_NAME} Rollback"),
        "atomic binary & runtime swap",
    );
    println!();
    println!(
        "  {} {}",
        cli::dim("Usage:"),
        cli::bold("donsetch rollback [--check]")
    );
    println!("         {}", cli::dim("alias: donsetch --rollback"));
    println!();
    println!("  Options");
    cli::print_opt(
        "--check, -c",
        "Inspect the stored backup version and size without modifying disk",
    );
    println!();
    println!(
        "  {}",
        cli::dim(
            "Swaps the active binary (and any sibling runtime libraries) with the `.bak` saved\n  by `donsetch update`. Running `donsetch rollback` again rolls forward."
        )
    );
    println!();
    cli::print_nav(&[
        ("donsetch update", "fetch newest release"),
        ("donsetch version", "check active binary"),
    ]);
}

#[allow(clippy::needless_borrows_for_generic_args)]
pub fn run(args: &[String]) {
    crate::cli::args::Spec::new(
        "rollback",
        "donsetch rollback [--check]",
        &["--check", "-c"],
    )
    .reject(&args[2..]);
    let check_only = args[2..].iter().any(|a| a == "--check" || a == "-c");

    cli::init();
    cli::print_header(&format!("{DISPLAY_NAME} Rollback"), "backup swap");
    println!();

    let current = env!("CARGO_PKG_VERSION");
    cli::print_kv("installed", &cli::bold(current));

    // ── Binary path (canonicalized so symlinks remain intact) ─

    let exe = match std::env::current_exe() {
        Ok(p) => std::fs::canonicalize(&p).unwrap_or(p),
        Err(e) => {
            cli::check_fail("binary path", &e.to_string(), "");
            std::process::exit(1);
        }
    };

    let exe_dir = exe.parent().unwrap_or_else(|| Path::new("."));
    cli::print_kv("location", &cli::dim(&exe.display().to_string()));

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
        cli::check_fail(
            "no backup found",
            &bak_path.display().to_string(),
            "Run `donsetch update` first; a rollback backup is saved automatically.",
        );
        std::process::exit(1);
    }

    // ── Read backup version ──────────────────────────────────

    let bak_ver = std::fs::read_to_string(&bak_ver_path)
        .unwrap_or_default()
        .trim()
        .to_string();

    if bak_ver.is_empty() {
        cli::print_kv("backup", "unknown version");
    } else if bak_ver == current && !check_only {
        println!();
        cli::check_warn(
            "same version",
            &format!("backup holds {current}; nothing to roll back to"),
        );
        std::process::exit(1);
    } else {
        cli::print_kv(
            "backup",
            &format!(
                "{} {} {}",
                cli::dim(current),
                cli::crimson("\u{21BA}"),
                cli::green(&bak_ver)
            ),
        );
    }

    // ── Integrity check ──────────────────────────────────────

    let bak_meta = match std::fs::metadata(&bak_path) {
        Ok(m) => m,
        Err(e) => {
            println!();
            cli::check_fail("backup metadata", &e.to_string(), "");
            std::process::exit(1);
        }
    };

    if bak_meta.len() < 1_000_000 {
        println!();
        cli::check_fail(
            "backup corrupt",
            &format!("{} bytes", bak_meta.len()),
            "Run `donsetch update --force` to download a fresh copy.",
        );
        std::process::exit(1);
    }

    cli::print_kv("backup size", &cli::format_bytes(bak_meta.len()));

    if check_only {
        println!();
        cli::check_pass(
            "backup ready",
            &format!(
                "{} ({})",
                if bak_ver.is_empty() {
                    "previous version"
                } else {
                    &bak_ver
                },
                cli::format_bytes(bak_meta.len())
            ),
        );
        println!();
        cli::print_footer();
        return;
    }

    // ── Swap ──────────────────────────────────────────────────

    cli::print_section("Swap");

    #[cfg(unix)]
    {
        match swap_unix(&exe, &bak_path, &bak_ver_path, current) {
            Ok(Some(warning)) => cli::check_warn("roll-forward backup", &warning),
            Ok(None) => {}
            Err(e) => {
                let hint = if e.contains("Permission")
                    || e.contains("denied")
                    || e.contains("access")
                    || e.contains("read-only")
                {
                    "Try: sudo donsetch rollback"
                } else {
                    ""
                };
                cli::check_fail("swap failed", &e, hint);
                std::process::exit(1);
            }
        }

        restore_sibling_libs(exe_dir);
    }

    #[cfg(windows)]
    {
        let tmp = exe_dir.join(".donsetch.rollback.tmp");
        recover_stale_rollback_tmp(&exe, &tmp);

        if let Err(e) = std::fs::rename(&exe, &tmp).map_err(|e| e.to_string()) {
            cli::check_fail("rename current", &e, "");
            std::process::exit(1);
        }

        if let Err(e) = std::fs::copy(&bak_path, &exe).map_err(|e| {
            let _ = std::fs::rename(&tmp, &exe);
            e.to_string()
        }) {
            cli::check_fail("copy backup", &e, "");
            std::process::exit(1);
        }

        match std::fs::rename(&tmp, &bak_path) {
            Ok(()) => {
                let _ = std::fs::write(&bak_ver_path, current);
            }
            Err(e) => {
                cli::check_warn(
                    "roll-forward backup",
                    &format!("rollback OK, could not save roll-forward backup: {e}"),
                );
            }
        }

        restore_sibling_libs(exe_dir);
    }

    if !bak_ver.is_empty() {
        crate::cli::update::sync_npm_version_stamp(&exe, &bak_ver);
    }
    crate::cli::update::clear_macos_quarantine(&exe);

    cli::check_pass(
        "binary swapped",
        "previous version restored; current saved for roll-forward",
    );

    println!();
    cli::print_footer();
    if !bak_ver.is_empty() {
        println!("  {} Rolled back {current} -> {bak_ver}", cli::icon_pass());
    } else {
        println!("  {} Rolled back to previous version", cli::icon_pass());
    }
}

/// Put every sibling runtime `update` staged back beside the
/// previous binary. One list (`SIBLING_LIBS`) drives this on every
/// platform so a rolled-back binary never keeps a newer runtime
/// (4.3.7's Windows branch restored `pdfium.dll` only).
fn restore_sibling_libs(exe_dir: &std::path::Path) {
    for name in crate::cli::update::SIBLING_LIBS {
        if let Err(e) = crate::cli::update::swap_sibling_lib(exe_dir, name) {
            println!(
                "  {} Rolled back the binary, but could not restore {name}: {e}",
                cli::icon_warn()
            );
        }
    }
}

/// Recover a stale `.donsetch.rollback.tmp` from an interrupted
/// rollback. On Windows the swap renames the running exe aside to
/// `tmp` and only then copies the backup into place: a crash in
/// between leaves the ONLY copy of the current binary at `tmp` and
/// nothing at the exe path. The next rollback used to `remove_file`
/// that temp outright -- destroying the last copy before discovering
/// there was no exe to rename. Put it back instead; a temp next to a
/// healthy exe is just debris and is dropped.
#[cfg(any(windows, test))]
fn recover_stale_rollback_tmp(exe: &Path, tmp: &Path) {
    if !exe.exists() && tmp.exists() {
        let _ = std::fs::rename(tmp, exe);
    } else {
        let _ = std::fs::remove_file(tmp);
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

#[cfg(test)]
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

    // restore_sibling_libs must cover every name the updater stages.
    // 4.3.7's Windows rollback restored pdfium.dll only, so a
    // rolled-back binary kept a newer onnxruntime.dll. This is the
    // red proof: drop a name from the restore list and it fails.
    #[test]
    fn restore_sibling_libs_covers_every_staged_runtime() {
        let dir = scratch("restore-libs");
        for name in crate::cli::update::SIBLING_LIBS {
            std::fs::write(dir.join(name), format!("new-{name}")).unwrap();
            std::fs::write(dir.join(format!("{name}.bak")), format!("old-{name}")).unwrap();
        }

        restore_sibling_libs(&dir);

        for name in crate::cli::update::SIBLING_LIBS {
            assert_eq!(
                read(&dir.join(name)),
                format!("old-{name}"),
                "{name} not restored"
            );
            assert_eq!(
                read(&dir.join(format!("{name}.bak"))),
                format!("new-{name}"),
                "{name} roll-forward missing"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // An interrupted rollback leaves the ONLY copy of the current
    // binary at `.donsetch.rollback.tmp` (the window between renaming
    // the exe aside and copying the backup into place). The next
    // rollback used to delete that temp outright, destroying the last
    // copy. Red proof: the old `remove_file` leaves the exe missing.
    #[test]
    fn a_stale_rollback_tmp_is_restored_when_the_exe_is_gone() {
        let dir = scratch("recover-gone");
        let exe = dir.join("donsetch.exe");
        let tmp = dir.join(".donsetch.rollback.tmp");
        std::fs::write(&tmp, "current-bin").unwrap();

        recover_stale_rollback_tmp(&exe, &tmp);

        assert_eq!(read(&exe), "current-bin", "last copy of the binary lost");
        assert!(!tmp.exists(), "temp left behind after recovery");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_rollback_tmp_is_dropped_when_the_exe_is_present() {
        let dir = scratch("recover-present");
        let exe = dir.join("donsetch.exe");
        let tmp = dir.join(".donsetch.rollback.tmp");
        std::fs::write(&exe, "current-bin").unwrap();
        std::fs::write(&tmp, "debris").unwrap();

        recover_stale_rollback_tmp(&exe, &tmp);

        assert_eq!(read(&exe), "current-bin");
        assert!(!tmp.exists(), "debris kept next to a healthy exe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
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
    #[cfg(unix)]
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
