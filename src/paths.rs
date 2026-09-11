//! Cross-platform path helpers for DonSeTch persistent state.
//!
//! All persistent state lives under one per-platform cache dir:
//!   Linux:   $XDG_CACHE_HOME/donsetch  or  ~/.cache/donsetch
//!   macOS:   ~/Library/Caches/donsetch
//!   Windows: %LOCALAPPDATA%\donsetch
//!
//! Backed by the `dirs` crate (zero transitive deps, the de
//! facto standard for this exact problem).

use std::path::{Component, Path, PathBuf};

/// The DonSeTch cache root. Falls back to the system temp dir
/// if the platform reports no cache directory (shouldn't happen
/// on any real user account).
pub fn cache_dir() -> PathBuf {
    // Test/container override: everything stateful hangs off this
    // one root, so redirecting it isolates a whole daemon cleanly.
    // The env var is read per call on purpose: tests set it late.
    if let Some(d) = std::env::var_os("DONSETCH_CACHE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    // Layered config fallback ([paths] cache_dir in donsetch.toml).
    let configured = crate::config::cfg().paths.cache_dir.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("donsetch")
}

/// Screenshots live under the cache root: `cache_dir()/screenshots`.
pub fn screenshots_dir() -> PathBuf {
    cache_dir().join("screenshots")
}

/// Centralized, safe screenshot output-path helper.
///
/// Contract:
/// - `input` must be non-empty.
/// - Relative names are resolved below `cache_dir()/screenshots`
///   (e.g. `"shot.png"` → `cache/screenshots/shot.png`,
///   `"a/b/c.png"` → `cache/screenshots/a/b/c.png`).
/// - Absolute paths are accepted **only** when already strictly below
///   the canonical `screenshots` root.
/// - Rejects `ParentDir` (`..`) components anywhere (path traversal).
/// - Rejects symlinked / outside parents: the nearest existing ancestor
///   of the resolved path is canonicalized and must still be under the
///   canonical screenshots root; otherwise the path is rejected.
/// - Rejects an existing destination whose `symlink_metadata` file type
///   is a symlink, and rejects an existing destination that
///   canonicalizes outside the canonical screenshots root.
/// - Creates only the intended parent directories (no pre-creation of
///   attacker-controlled absolute parents outside the root).
///
/// Returns the validated, absolute destination path on success.
pub fn resolve_screenshot_path(input: &str) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("screenshot path is empty : provide a filename".into());
    }
    let p = PathBuf::from(trimmed);

    // Reject ParentDir anywhere.
    for comp in p.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(format!(
                "screenshot path traversal rejected: `{trimmed}` contains `..`"
            ));
        }
    }

    // Reject NUL bytes or interior newlines that could confuse display.
    if trimmed.contains('\0') {
        return Err("screenshot path contains NUL".into());
    }

    // Resolve to absolute destination. has_root(), not
    // is_absolute(): a Windows path rooted without a drive prefix
    // (e.g. `/tmp/x.png` as entered, or `\tmp\x.png`) is not
    // "absolute" by is_absolute(), yet PathBuf::join replaces the
    // base entirely for any rooted path: without has_root() such an
    // input skips the early under-root rejection below. The
    // canonical-frame check catches it anyway today; this makes the
    // first line of defense honest instead of accidental.
    let screenshots = screenshots_dir();
    let is_absolute = p.has_root();
    let dest = if is_absolute { p } else { screenshots.join(&p) };

    // Ensure screenshots root exists so we have a canonical base.
    std::fs::create_dir_all(&screenshots)
        .map_err(|e| format!("failed to create screenshots dir: {e}"))?;
    let canon_root = std::fs::canonicalize(&screenshots).unwrap_or_else(|_| screenshots.clone());

    // For absolute inputs, the raw dest must already be prefixed by the
    // screenshots root (string prefix check before canonicalization, to
    // fail fast on clearly outside paths).
    if is_absolute && !is_below_root(&dest, &canon_root) && !is_below_root(&dest, &screenshots) {
        return Err(format!(
            "absolute screenshot path outside allowed root: `{trimmed}` must be below {}",
            canon_root.display()
        ));
    }

    // Symlink / outside-parent check: canonicalize nearest existing
    // ancestor and ensure it is still under the root.
    let parent = dest
        .parent()
        .ok_or_else(|| "screenshot path has no parent".to_string())?;
    let nearest_existing = nearest_existing_ancestor(parent);
    if let Some(existing) = nearest_existing {
        let canon_parent = std::fs::canonicalize(&existing).unwrap_or_else(|_| existing.clone());
        if !is_below_root(&canon_parent, &canon_root) {
            return Err(format!(
                "screenshot path parent escapes allowed root (symlink/outside): `{trimmed}` -> {} not under {}",
                canon_parent.display(),
                canon_root.display()
            ));
        }
        // Also verify the existing ancestor is the root itself or a child.
        // If the existing ancestor is e.g. `/tmp`, its canonical won't be under root.
    } else {
        // No existing ancestor at all (should not happen because screenshots
        // root exists). Fail closed.
        return Err(format!(
            "screenshot path has no existing ancestor under allowed root: `{trimmed}`"
        ));
    }

    // Create only the intended destination parent (which we have already
    // validated to be under the root).
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("failed to create screenshot parent dir: {e}"))?;

    // Final post-create canonical check: parent must still be under root
    // (catches race where a symlink was created between checks).
    if let Ok(canon_final) = std::fs::canonicalize(parent)
        && !is_below_root(&canon_final, &canon_root)
    {
        return Err(format!(
            "screenshot path parent escapes allowed root after creation: `{trimmed}`"
        ));
    }

    // Harden: reject existing destination that is itself a symlink, or
    // canonicalizes outside the screenshots root. `symlink_metadata`
    // does not follow the final symlink, so a symlink file is caught
    // even if its target is inside the root.
    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() {
            return Err(format!("screenshot destination is a symlink: `{trimmed}`"));
        }
        if let Ok(canon_dest) = std::fs::canonicalize(&dest)
            && !is_below_root(&canon_dest, &canon_root)
        {
            return Err(format!(
                "screenshot destination escapes allowed root: `{trimmed}` -> {} not under {}",
                canon_dest.display(),
                canon_root.display()
            ));
        }
    }

    Ok(dest)
}

fn is_below_root(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur: &Path = path;
    loop {
        if cur.exists() {
            return Some(cur.to_path_buf());
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_ends_with_donsetch() {
        assert!(cache_dir().ends_with("donsetch"));
    }

    #[test]
    fn screenshot_valid_relative_name() {
        let p = resolve_screenshot_path("shot.png").expect("valid relative");
        assert!(
            p.starts_with(screenshots_dir())
                || p.starts_with(
                    std::fs::canonicalize(screenshots_dir()).unwrap_or_else(|_| screenshots_dir())
                )
        );
        assert!(p.ends_with("shot.png"));
        // cleanup
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn screenshot_rejects_parent_traversal() {
        let err = resolve_screenshot_path("../evil.png").unwrap_err();
        assert!(
            err.contains("..") || err.contains("traversal"),
            "got: {err}"
        );
        let err2 = resolve_screenshot_path("a/../../b.png").unwrap_err();
        assert!(
            err2.contains("..") || err2.contains("traversal"),
            "got: {err2}"
        );
    }

    #[test]
    fn screenshot_rejects_absolute_outside() {
        let outside = std::env::temp_dir().join("outside-evil.png");
        let err = resolve_screenshot_path(outside.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("outside") || err.contains("allowed root"),
            "got: {err}"
        );
        let err2 = resolve_screenshot_path("/etc/passwd").unwrap_err();
        assert!(
            err2.contains("outside") || err2.contains("allowed root"),
            "got: {err2}"
        );
    }

    #[test]
    fn screenshot_valid_cache_path() {
        let valid = screenshots_dir().join("sub").join("ok.png");
        let got = resolve_screenshot_path(valid.to_str().unwrap()).expect("valid cache path");
        assert!(
            got.starts_with(screenshots_dir())
                || got.starts_with(
                    std::fs::canonicalize(screenshots_dir()).unwrap_or_else(|_| screenshots_dir())
                )
        );
        // cleanup
        let _ = std::fs::remove_file(&got);
        let _ = std::fs::remove_dir_all(screenshots_dir().join("sub"));
    }

    #[test]
    fn screenshot_rejects_empty() {
        assert!(resolve_screenshot_path("").is_err());
        assert!(resolve_screenshot_path("   ").is_err());
    }

    #[test]
    fn screenshot_rejects_symlinked_parent() {
        // Create a temp dir outside the screenshots root and symlink a
        // child dir inside screenshots to it. The helper must reject
        // writing through that symlink.
        let screenshots = screenshots_dir();
        let _ = std::fs::create_dir_all(&screenshots);
        let outside = std::env::temp_dir().join("donsetch-test-outside");
        let _ = std::fs::create_dir_all(&outside);
        let link = screenshots.join("evil-link");
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&link);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let _ = symlink(&outside, &link);
            if link.exists() {
                let bad = link.join("shot.png");
                let err = resolve_screenshot_path(bad.to_str().unwrap()).unwrap_err();
                assert!(
                    err.contains("outside")
                        || err.contains("escapes")
                        || err.contains("allowed root"),
                    "got: {err}"
                );
                let _ = std::fs::remove_file(&link);
                let _ = std::fs::remove_dir_all(&outside);
            }
        }
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn screenshot_rejects_existing_symlink_destination() {
        // An existing destination file that is itself a symlink (even to a
        // file outside the root) must be rejected via symlink_metadata.
        let screenshots = screenshots_dir();
        let _ = std::fs::create_dir_all(&screenshots);
        let outside_file = std::env::temp_dir().join("donsetch-test-outside-file.png");
        let _ = std::fs::write(&outside_file, b"outside");
        let dest_link = screenshots.join("evil-dest-symlink.png");
        let _ = std::fs::remove_file(&dest_link);
        let _ = std::fs::remove_dir_all(&dest_link);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let _ = symlink(&outside_file, &dest_link);
            if let Ok(meta) = std::fs::symlink_metadata(&dest_link)
                && meta.file_type().is_symlink()
            {
                // relative input resolving to the symlink
                let err = resolve_screenshot_path("evil-dest-symlink.png").unwrap_err();
                assert!(
                    err.contains("symlink")
                        || err.contains("outside")
                        || err.contains("escapes")
                        || err.contains("allowed root"),
                    "got: {err}"
                );
                // absolute input resolving to the same symlink
                let err2 = resolve_screenshot_path(dest_link.to_str().unwrap()).unwrap_err();
                assert!(
                    err2.contains("symlink")
                        || err2.contains("outside")
                        || err2.contains("escapes")
                        || err2.contains("allowed root"),
                    "got: {err2}"
                );
                // sanity: canonical destination is outside the screenshots root
                if let Ok(canon_dest) = std::fs::canonicalize(&dest_link) {
                    let canon_root =
                        std::fs::canonicalize(&screenshots).unwrap_or_else(|_| screenshots.clone());
                    assert!(
                        !is_below_root(&canon_dest, &canon_root),
                        "canon_dest {canon_dest:?} should be outside {canon_root:?}"
                    );
                }
            }
        }
        let _ = std::fs::remove_file(&dest_link);
        let _ = std::fs::remove_file(&outside_file);
    }
}
