//! Small, shared parsers for process-level runtime configuration.

use std::ffi::OsStr;

/// Read a security-sensitive opt-in flag from the environment.
///
/// Only explicit affirmative values enable the flag. Matching is
/// ASCII-case-insensitive and ignores surrounding ASCII/Unicode
/// whitespace; missing, non-Unicode, false, and unknown values all
/// fail closed.
pub(crate) fn env_flag(name: &str) -> bool {
    env_flag_value(std::env::var_os(name).as_deref())
}

/// Write a file that holds credentials (API keys, proxy passwords)
/// so that it is owner-only from the moment it exists.
///
/// `fs::write` followed by `set_permissions` creates the file at the
/// umask default (0644 on most systems) and only tightens it after
/// the secret has already landed: a world-readable window, and a
/// world-readable file for good if anything fails in between. This
/// opens with mode 0600 up front, then also re-applies 0600 for the
/// case where the file already existed with looser permissions
/// (`mode` on open only affects creation). Non-Unix: plain write.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.flush()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

fn env_flag_value(value: Option<&OsStr>) -> bool {
    value
        .and_then(OsStr::to_str)
        .map(str::trim)
        .is_some_and(|value| {
            value.eq_ignore_ascii_case("1")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("on")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_flags_accept_only_explicit_true_values() {
        for value in ["1", "true", "TRUE", "on", "ON", " true ", "\t1\n"] {
            assert!(env_flag_value(Some(OsStr::new(value))), "{value:?}");
        }

        for value in [
            "",
            "0",
            "false",
            "FALSE",
            "off",
            "OFF",
            "yes",
            "no",
            "enabled",
            "anything",
            " true-ish ",
        ] {
            assert!(!env_flag_value(Some(OsStr::new(value))), "{value:?}");
        }

        assert!(!env_flag_value(None));
    }

    #[cfg(unix)]
    #[test]
    fn opt_in_flags_reject_non_unicode_values() {
        use std::os::unix::ffi::OsStrExt;

        assert!(!env_flag_value(Some(OsStr::from_bytes(b"true\xff"))));
    }

    #[cfg(unix)]
    fn mode_of(p: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    // A fresh file must be 0600 from the moment it exists (not
    // created at the umask default and chmod'ed afterwards, which
    // leaves a world-readable window and a world-readable file on
    // any crash in between).
    #[cfg(unix)]
    #[test]
    fn write_private_creates_owner_only() {
        let dir =
            std::env::temp_dir().join(format!("donsetch-write-private-{}-new", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secret.json");

        write_private(&p, b"{\"key\":\"s3cr3t\"}").unwrap();

        assert_eq!(mode_of(&p), 0o600);
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"key\":\"s3cr3t\"}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // An existing world-readable file (e.g. an export written by an
    // older build) is tightened as well as truncated and rewritten.
    #[cfg(unix)]
    #[test]
    fn write_private_tightens_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "donsetch-write-private-{}-existing",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secret.json");
        std::fs::write(&p, "old and longer content").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&p, b"new").unwrap();

        assert_eq!(mode_of(&p), 0o600);
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
