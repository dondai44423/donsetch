//! `donsetch stop` : kill orphaned Chrome instances and clean up
//! stale lock files. Use after a crash or when Chrome processes
//! from a previous session are still resident.

pub fn run() {
    let profile = crate::ghost::profile_dir();

    #[cfg(unix)]
    {
        // pkill -f matches a POSIX ERE against the full command line.
        // The profile path must be escaped: a metacharacter in it (`.`
        // in `~/.cache`, `+`/`(`/`[` in a user name) silently loosens
        // the match -- orphans survive -- or over-matches into other
        // processes' command lines.
        let pattern = format!("user-data-dir={}", escape_ere(&profile.display().to_string()));
        // Kills every Chrome process using the ghost profile,
        // including renderers and GPU processes that share the
        // --user-data-dir argument.
        let out = std::process::Command::new("pkill")
            .args(["-9", "-f", pattern.as_str()])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                eprintln!("[ghost] killed orphaned Chrome instances");
            }
            Ok(_) => {
                // pkill exit 1 = no processes matched: not an error.
                eprintln!("[ghost] no orphaned Chrome instances found");
            }
            Err(_) => {
                eprintln!("[ghost] pkill not available, checking manually");
            }
        }
    }

    // Clean up stale lock files regardless of platform.
    for f in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(f));
    }
    let _ = std::fs::remove_file(crate::paths::cache_dir().join("ghost-profile.lock"));

    // Sweep temp profiles left behind by a crash.
    sweep_temp_profiles(&std::env::temp_dir());

    eprintln!("[ghost] cleaned stale locks and temp profiles");
}

/// Escape POSIX extended-regular-expression metacharacters so a
/// filesystem path is matched literally by `pkill -f`.
fn escape_ere(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Remove crash-leftover `donsetch-ghost-<pid>` temp profiles whose
/// owning process is gone.
///
/// The suffix is the owning process's pid (see ghost/mod.rs: a second
/// session that loses the shared-profile flock runs Chrome against a
/// throwaway `donsetch-ghost-<pid>` profile). That profile is LIVE for
/// as long as the process runs: deleting it mid-session rips the
/// user-data-dir out from under a running Chrome. Only profiles whose
/// pid is gone are debris.
fn sweep_temp_profiles(temp_dir: &std::path::Path) {
    const TEMP_PREFIX: &str = "donsetch-ghost-";
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(pid_str) = name.strip_prefix(TEMP_PREFIX) else {
            continue;
        };
        if let Some(pid) = pid_str.parse::<u32>().ok().filter(|p| *p > 0)
            && process_is_alive(pid)
        {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// Is a pid still running? Used to tell a crash leftover from a live
/// session's temp profile.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0) probes existence: 0 = alive, EPERM = alive but
        // not ours, ESRCH = gone.
        let r = unsafe { libc::kill(pid as i32, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation as fnd;
        use windows_sys::Win32::System::Threading as thr;
        // SAFETY: OpenProcess with query-only rights; the handle is
        // closed before returning.
        unsafe {
            let h = thr::OpenProcess(thr::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                false
            } else {
                fnd::CloseHandle(h);
                true
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("donsetch-stop-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    // A concurrent session's fallback profile is named after its
    // owner's pid and is LIVE. The old sweep deleted every
    // `donsetch-ghost-*` dir unconditionally, ripping the
    // user-data-dir out from under a running Chrome. Red proof: the
    // old code removes this dir too.
    #[test]
    fn sweep_keeps_a_live_sessions_temp_profile() {
        let dir = scratch("live");
        let live = dir.join(format!("donsetch-ghost-{}", std::process::id()));
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("Cookies"), "live").unwrap();

        sweep_temp_profiles(&dir);

        assert!(
            live.join("Cookies").exists(),
            "a live session's temp profile was deleted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn sweep_removes_a_dead_sessions_temp_profile() {
        let dir = scratch("dead");
        // A pid that has exited: spawn and reap a child, then use it.
        let mut child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        let dead_pid = child.id();
        child.wait().expect("wait");
        let dead = dir.join(format!("donsetch-ghost-{dead_pid}"));
        std::fs::create_dir_all(&dead).unwrap();

        sweep_temp_profiles(&dir);

        assert!(!dead.exists(), "crash leftover was kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // pkill -f treats the pattern as an ERE: the profile path must be
    // matched literally. Red proof: `.` unescaped matches any
    // character, so the old pattern matched a path that is NOT the
    // profile path.
    #[test]
    fn pkill_pattern_escapes_regex_metacharacters() {
        let got = escape_ere("/home/u/.cache/donsetch/ghost-profile");
        assert_eq!(
            got,
            "/home/u/\\.cache/donsetch/ghost-profile",
            "dots must be literal"
        );
        let got = escape_ere("a+b(c)[d]{e}|f^g$h*i?j\\k");
        assert_eq!(
            got,
            "a\\+b\\(c\\)\\[d\\]\\{e\\}\\|f\\^g\\$h\\*i\\?j\\\\k"
        );
    }
}
