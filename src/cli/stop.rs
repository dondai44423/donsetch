//! `donsetch stop` : kill orphaned browser instances and clean up
//! stale lock files and crash-leftover profiles. Use after a crash or
//! when browser processes from a previous session are still resident.

pub fn run(args: &[String]) {
    crate::cli::args::Spec::new("stop", "donsetch stop", &[]).reject(&args[2..]);

    #[cfg(unix)]
    {
        // pkill -f matches a POSIX ERE against the full command line.
        // The paths must be escaped: a metacharacter in one (`.` in
        // `~/.cache`, `+`/`(`/`[` in a user name) silently loosens
        // the match -- orphans survive -- or over-matches into other
        // processes' command lines.
        let auth = escape_ere(
            &crate::paths::cache_dir()
                .join("auth-profile")
                .display()
                .to_string(),
        );
        let browse = escape_ere(
            &std::env::temp_dir()
                .join("donsetch-browse-")
                .display()
                .to_string(),
        );
        let pattern = format!("user-data-dir=({auth}|{browse})");
        // Kills every browser process bound to those profiles,
        // including renderers and GPU processes that share the
        // --user-data-dir argument.
        let out = std::process::Command::new("pkill")
            .args(["-9", "-f", pattern.as_str()])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                crate::diag!("[browse] killed orphaned browser instances");
            }
            Ok(_) => {
                // pkill exit 1 = no processes matched: not an error.
                crate::diag!("[browse] no orphaned browser instances found");
            }
            Err(_) => {
                crate::diag!("[browse] pkill not available, checking manually");
            }
        }
    }

    // Clean up stale lock files regardless of platform.
    let auth_profile = crate::paths::cache_dir().join("auth-profile");
    for f in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(auth_profile.join(f));
    }
    let _ = std::fs::remove_file(crate::paths::cache_dir().join("auth-profile.lock"));
    // Legacy leftover from the ghost era: never written now, but a
    // pre-v5 crash may still have left the lock behind.
    let _ = std::fs::remove_file(crate::paths::cache_dir().join("ghost-profile.lock"));

    // Sweep temp profiles left behind by a crash.
    sweep_temp_profiles(&std::env::temp_dir());

    crate::diag!("[browse] cleaned stale locks and temp profiles");
}

/// Escape POSIX extended-regular-expression metacharacters so a
/// filesystem path is matched literally by `pkill -f`.
#[cfg(any(unix, test))]
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

/// Remove crash-leftover `donsetch-browse-<pid>-*` (and legacy
/// `donsetch-ghost-<pid>`) temp profiles whose owning process is gone.
///
/// The suffix starts with the owning process's pid (see
/// browser/mod.rs: every capture gets a throwaway
/// `donsetch-browse-<pid>-<nonce>-<seq>` profile). That profile is
/// LIVE for as long as the process runs: deleting it mid-session
/// rips the user-data-dir out from under a running browser. Only
/// profiles whose pid is gone are debris.
fn sweep_temp_profiles(temp_dir: &std::path::Path) {
    const TEMP_PREFIXES: [&str; 2] = ["donsetch-browse-", "donsetch-ghost-"];
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = TEMP_PREFIXES
            .iter()
            .find_map(|prefix| name.strip_prefix(prefix))
        else {
            continue;
        };
        let pid_str = rest.split('-').next().unwrap_or(rest);
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
        let dir =
            std::env::temp_dir().join(format!("donsetch-stop-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    // A live session's capture profile is named after its owner's
    // pid and is LIVE. Deleting it mid-session rips the
    // user-data-dir out from under a running browser.
    #[test]
    fn sweep_keeps_a_live_sessions_temp_profile() {
        let dir = scratch("live");
        let live = dir.join(format!("donsetch-browse-{}-0-0", std::process::id()));
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
        let dead = dir.join(format!("donsetch-browse-{dead_pid}-0-0"));
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
        let got = escape_ere("/home/u/.cache/donsetch/auth-profile");
        assert_eq!(
            got, "/home/u/\\.cache/donsetch/auth-profile",
            "dots must be literal"
        );
        let got = escape_ere("a+b(c)[d]{e}|f^g$h*i?j\\k");
        assert_eq!(got, "a\\+b\\(c\\)\\[d\\]\\{e\\}\\|f\\^g\\$h\\*i\\?j\\\\k");
    }
}
