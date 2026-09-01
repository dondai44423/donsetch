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
}
