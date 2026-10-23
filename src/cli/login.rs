//! `donsetch login` :  interactive, secure session capture.
//!
//! Design rules (see src/auth.rs for the security model):
//! - We open a REAL browser on YOUR display with a dedicated profile.
//!   You type your credentials into it. We capture nothing: no
//!   keystrokes, no screenshots, no CDP until you press Enter.
//! - After Enter: cookies for the target domain (or newly appeared
//!   cookies, in multi-site mode) are harvested, filtered to
//!   session-worthy ones, and stored in the existing vault, which
//!   both tier-1 fetches and tier-2 renders already replay.
//! - Cookie values live only in the vault; the registry
//!   (auth-state.json) is masked metadata.

use std::io::{IsTerminal, Write};

use tokio::io::AsyncBufReadExt;

use crate::auth::{self, AuthRegistry};
use crate::state::CookieRecord;

pub async fn run(args: &[String]) -> Result<(), String> {
    // Validation first: an unknown flag must not open a browser.
    crate::cli::args::Spec::new(
        "login",
        "donsetch login [domain] [--list|--status D|--logout D|--import F]",
        &["--list", "--fresh", "--no-probe", "--yes", "-y"],
    )
    .with_valued(&["--logout", "--status", "--import"])
    .reject(&args[2..]);

    // ── subcommand parse (manual, matching keys.rs style) ──
    let mut site: Option<String> = None;
    let mut action_flags: Vec<&str> = Vec::new();
    let mut import_file: Option<String> = None;
    let mut no_probe = false;
    let mut yes = false;
    let mut fresh = false;

    let mut it = args.iter().skip(2).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" | "list" => action_flags.push("list"),
            "--logout" | "logout" => {
                action_flags.push("logout");
                if let Some(&v) = it.peek()
                    && !v.starts_with("--")
                {
                    site = Some(v.to_string());
                    it.next();
                }
            }
            "--status" | "status" => {
                action_flags.push("status");
                if let Some(&v) = it.peek()
                    && !v.starts_with("--")
                {
                    site = Some(v.to_string());
                    it.next();
                }
            }
            "--import" | "import" => {
                action_flags.push("import");
                if let Some(&v) = it.peek()
                    && !v.starts_with("--")
                {
                    import_file = Some(v.to_string());
                    it.next();
                }
            }
            "--no-probe" => no_probe = true,
            "--yes" | "-y" => yes = true,
            "--fresh" => fresh = true,
            "--help" | "-h" | "help" => {
                help();
                return Ok(());
            }
            other if other.starts_with("--") => {
                crate::diag!("donsetch login: unknown flag {other}\n");
                help();
                return Err("invalid login arguments".into());
            }
            _ => {
                if site.is_none() {
                    site = Some(a.clone());
                } else {
                    crate::diag!("donsetch login: unexpected argument '{a}'\n");
                    help();
                    return Err("invalid login arguments".into());
                }
            }
        }
    }

    if action_flags.len() > 1 {
        return Err("choose one login action".into());
    }
    let action = action_flags.first().copied().unwrap_or("interactive");
    match action {
        "list" => list_sites(),
        "status" => {
            let Some(s) = site else {
                crate::diag!("donsetch login --status needs a domain");
                help();
                return Err("invalid login arguments".into());
            };
            print_status(&s);
        }
        "logout" => {
            let Some(s) = site else {
                crate::diag!("donsetch login --logout needs a domain");
                help();
                return Err("invalid login arguments".into());
            };
            logout(&s, yes)?;
        }
        "import" => {
            let Some(f) = import_file else {
                crate::diag!("donsetch login --import needs a cookies.txt file");
                help();
                return Err("invalid login arguments".into());
            };
            import(&f, site.as_deref(), !no_probe)?;
        }
        _ => {
            interactive(site.as_deref(), fresh, !no_probe).await?;
        }
    }
    Ok(())
}

/// Usage text, shared by `donsetch login --help` and `donsetch help login`.
pub fn help() {
    println!("Usage:");
    println!("  donsetch login [domain]          Interactive: open a browser, you sign in,");
    println!("                                   press Enter here when done. The session is");
    println!("                                   stored and replayed by every later fetch.");
    println!("  donsetch login                   Multi-site mode: log into whatever you want,");
    println!("                                   press Enter, all new sessions are stored.");
    println!("  donsetch login --list            Show stored sessions (masked, never values).");
    println!("  donsetch login --status DOMAIN   Detail one session.");
    println!("  donsetch login --logout DOMAIN   Remove a session (vault + registry).");
    println!("  donsetch login --import FILE     Import a Netscape cookies.txt export,");
    println!("               [domain]            optionally restricted to one domain.");
    println!();
    println!("Options:");
    println!("  --fresh       Wipe the login browser profile first (shared machines).");
    println!("  --no-probe    Skip the named-domain reachability probe.");
    println!("  --yes | -y    Confirm --logout without prompting.");
    println!();
    println!("Credentials are typed into the browser only. donsetch never sees");
    println!("your password: no keystroke capture, no screenshots, no CDP before");
    println!("you press Enter. Stored cookie values live in the same 0600 vault");
    println!("the fetch engine already uses; the registry holds metadata only.");
}

// ── interactive flow ─────────────────────────────────────────

async fn interactive(site: Option<&str>, fresh: bool, probe: bool) -> Result<(), String> {
    if crate::config::cfg().state.no_disk_state {
        return Err("login requires disk state; state.no_disk_state is enabled".into());
    }
    AuthRegistry::load_checked()?;
    crate::state::validate_login_vault()?;
    // The other branches of `login` (`--list`, `--status`, `--logout`,
    // `--import`) are non-interactive on purpose; only this one needs a
    // human. Without a terminal it used to launch a real browser first and
    // discover the closed stdin afterwards, leaving nothing stored. State
    // errors come first: an unreadable vault is the operator's problem to
    // fix, and `unreadable_vault_refuses_login_before_launch_and_preserves_
    // existing_bytes` pins that message.
    if !std::io::stdin().is_terminal() {
        return Err(
            "needs a terminal: stdin is not a TTY, so there is nobody to press Enter \
             (nothing was stored). Non-interactive use: --list, --status D, --logout D, \
             --import FILE"
                .into(),
        );
    }
    let (open_url, key, probe_port) = match site {
        Some(s) => {
            let t = auth::normalize_target(s)?;
            (Some(t.browse_url), Some(t.key), t.probe_port)
        }
        None => (None, None, None),
    };

    // Browser resolution: stock Chromium, never the stealth backend.
    let resolved = crate::ghost::resolve_browser_without_download()
        .map_err(|e| format!("no usable browser: {e} (run `donsetch doctor` for guidance)"))?;
    // A login needs a visible browser. Headless machines use --import.
    if !crate::config::cfg().browser.login_force && !display_available() {
        return Err(
            "no display available: interactive login needs a visible browser. \
             On servers use `donsetch login --import cookies.txt [domain]` instead."
                .into(),
        );
    }

    // Dedicated profile: never the automation ghost-profile, so a
    // login session can never interleave with automated page work.
    let base = crate::paths::cache_dir();
    let profile_dir = base.join("auth-profile");
    let lock_path = base.join("auth-profile.lock");
    let browser_log = base.join("auth-browser.log");
    std::fs::create_dir_all(&base).map_err(|e| format!("cache dir: {e}"))?;
    let _lock = AuthLock::acquire(&lock_path)?;
    if profile_in_use(&profile_dir) {
        return Err(
            "login browser still owns the profile; close that browser before starting another login".into(),
        );
    }
    if fresh && profile_dir.exists() {
        std::fs::remove_dir_all(&profile_dir).map_err(|e| format!("--fresh wipe: {e}"))?;
    }
    std::fs::create_dir_all(&profile_dir).map_err(|e| format!("auth profile: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&profile_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("auth profile permissions: {e}"))?;
    }

    // Ephemeral debug port.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("port: {e}"))?;
        l.local_addr()
            .map_err(|e| format!("port addr: {e}"))?
            .port()
    };

    let mut log_options = std::fs::OpenOptions::new();
    log_options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let log_file = log_options
        .open(&browser_log)
        .map_err(|e| format!("browser log: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        log_file
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    let mut cmd = tokio::process::Command::new(&resolved.path);
    cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--no-crash-upload")
        .arg("--disable-breakpad")
        .arg(open_url.as_deref().unwrap_or("about:blank"));
    // Never inherit stealth/UA shaping: this is the user's login on
    // their stock browser.
    let mut child = cmd
        .stdout(std::process::Stdio::from(
            log_file.try_clone().map_err(|e| e.to_string())?,
        ))
        .stderr(std::process::Stdio::from(log_file))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("launch browser: {e}"))?;

    // Wait for the debugger endpoint.
    let endpoint = format!("http://127.0.0.1:{port}/json/version");
    println!("Starting browser: {}", resolved.path.display());
    let ws = retrieve_ws(&endpoint, 30_000).await?;

    // Compare against the vault; never attach CDP while credentials are typed.
    let baseline_res = crate::state::load_session_cookies();
    println!("Log in now. Come back here and press Enter to save the session.");
    if site.is_none() {
        println!("(multi-site mode: every NEW session you create gets saved)");
    }
    println!("Ctrl+C saves nothing to the vault and closes the browser.");

    // Enter saves. Ctrl+C discards and cleans up.
    let mut line = String::new();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err("cancelled: nothing was stored".into());
        }
        r = stdin.read_line(&mut line) => {
            if !matches!(r, Ok(n) if n > 0) {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err("stdin closed: nothing was stored".into());
            }
        }
    }

    let cdp = crate::ghost::cdp::Cdp::connect(&ws)
        .await
        .map_err(|e| format!("CDP attach: {e}; nothing stored"))?;
    let response = cdp
        .call(None, "Storage.getCookies", serde_json::json!({}))
        .await
        .map_err(|e| format!("cookie capture failed: {e}; nothing stored"))?;
    if response
        .get("cookies")
        .and_then(serde_json::Value::as_array)
        .is_none()
    {
        return Err("cookie capture returned no cookie inventory; nothing stored".into());
    }
    let final_res = crate::ghost::Ghost::parse_cdp_cookies(&response);

    // Close the browser cleanly (goodbye, cookies flushed).
    let _ = cdp.call(None, "Browser.close", serde_json::json!({})).await;
    if tokio::time::timeout(std::time::Duration::from_secs(6), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    // Filter + store.
    let kept: Vec<CookieRecord> = match key.as_deref() {
        Some(d) => final_res
            .into_iter()
            .filter(|c| auth::cookie_belongs_to(d, c.domain.trim_start_matches('.')))
            .collect(),
        None => {
            let before: std::collections::HashSet<(String, String, String, String)> = baseline_res
                .iter()
                .map(|c| {
                    (
                        c.domain.clone(),
                        c.path.clone(),
                        c.name.clone(),
                        c.value.clone(),
                    )
                })
                .collect();
            final_res
                .into_iter()
                .filter(|c| {
                    !before.contains(&(
                        c.domain.clone(),
                        c.path.clone(),
                        c.name.clone(),
                        c.value.clone(),
                    ))
                })
                .collect()
        }
    };

    if kept.is_empty() {
        println!("No new session cookies found: nothing stored.");
        return Ok(());
    }
    let (domains, probes) = auth::commit_login(&kept, probe, key.as_deref(), probe_port)?;
    if domains.is_empty() {
        println!("No usable login cookies found: nothing stored.");
        return Ok(());
    }
    println!("\nSaved:");
    for d in &domains {
        let reg = AuthRegistry::load();
        let names = reg
            .get(d)
            .map(|s| s.cookie_names.join(", "))
            .unwrap_or_default();
        println!("  {d}: {names}");
    }
    if probe {
        for (d, ok, note) in &probes {
            println!(
                "    probe {}{}: {note}",
                d,
                if *ok { " ✓" } else { " (unverified)" }
            );
        }
    }
    if probes.is_empty() && probe {
        println!("    (no probes: nothing stored)");
    }
    println!("\nLater fetches of these domains replay the session automatically.");
    println!("`donsetch login --list` shows stored sessions; `--logout` forgets them.");
    let _ = std::io::stdout().flush();
    Ok(())
}

async fn retrieve_ws(endpoint: &str, total_ms: u64) -> Result<String, String> {
    // Async client: this loop runs inside the tokio runtime driving
    // `donsetch login`, and `reqwest::blocking` panics (aborting the
    // process, since this crate builds with panic = "abort") when
    // built or dropped on a thread that already has a runtime context.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let start = std::time::Instant::now();
    loop {
        if let Ok(resp) = client.get(endpoint).send().await
            && let Ok(v) = resp.json::<serde_json::Value>().await
            && let Some(ws) = v
                .get("webSocketDebuggerUrl")
                .and_then(serde_json::Value::as_str)
        {
            return Ok(ws.to_string());
        }
        if start.elapsed().as_millis() as u64 > total_ms {
            return Err("timeout waiting for the browser".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn display_available() -> bool {
    if cfg!(target_os = "windows") || cfg!(target_os = "macos") {
        return true;
    }
    std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

// ── list / status / logout / import ──────────────────────────

fn list_sites() {
    let reg = AuthRegistry::load();
    let sites = reg.sorted();
    if sites.is_empty() {
        println!("No stored logins. Run `donsetch login <domain>` to add one.");
        return;
    }
    println!(
        "{:<24} {:<8} {:<24} {:<9} Names\n",
        "Domain", "Cookies", "Expires", "Verified"
    );
    for (d, s) in sites {
        let exp = match (s.expires_min, s.expires_max) {
            (None, None) => "session".to_string(),
            (Some(lo), Some(hi)) => {
                format!(
                    "{}-{}",
                    auth::fmt_expiry(Some(lo)),
                    auth::fmt_expiry(Some(hi))
                )
            }
            (Some(lo), _) => auth::fmt_expiry(Some(lo)),
            (_, Some(hi)) => auth::fmt_expiry(Some(hi)),
        };
        println!(
            "{:<24} {:<8} {:<24} {:<9} {}",
            d,
            s.cookie_count,
            exp,
            match s.verified {
                Some(true) => "yes",
                Some(false) => "no",
                None => "-",
            },
            s.cookie_names.join(", ")
        );
    }
}

fn print_status(domain: &str) {
    let key = match auth::normalize_target(domain) {
        Ok(t) => t.key,
        Err(e) => {
            crate::diag!("{e}");
            return;
        }
    };
    let reg = AuthRegistry::load();
    match reg.get(&key) {
        None => {
            println!("No stored login for {key}.");
            // Still worth checking the vault directly: login without
            // registry can happen after a manual import of an old
            // release.
            let vaulted = crate::state::load_session_cookies()
                .into_iter()
                .filter(|c| auth::cookie_belongs_to(&key, c.domain.trim_start_matches('.')))
                .count();
            if vaulted > 0 {
                println!("(vault holds {vaulted} cookie(s); registry is empty)");
            }
        }
        Some(s) => {
            println!("Domain:    {key}");
            println!("Cookies:   {}", s.cookie_count);
            println!(
                "Expires:   {}",
                match (s.expires_min, s.expires_max) {
                    (None, None) => "session".to_string(),
                    (Some(lo), Some(hi)) => format!(
                        "{} .. {}",
                        auth::fmt_expiry(Some(lo)),
                        auth::fmt_expiry(Some(hi))
                    ),
                    (Some(lo), _) => auth::fmt_expiry(Some(lo)),
                    (_, Some(hi)) => auth::fmt_expiry(Some(hi)),
                }
            );
            println!(
                "Verified:  {}",
                match s.verified {
                    Some(true) => "yes (post-login probe passed)",
                    Some(false) => "no (probe saw a login wall: re-login)",
                    None => "unverified (a public home page cannot prove authentication)",
                }
            );
            println!("Last:     {}", s.last_login);
            if !s.cookie_names.is_empty() {
                println!("Names:    {}", s.cookie_names.join(", "));
            }
        }
    }
}

fn logout(domain: &str, assume_yes: bool) -> Result<(), String> {
    let key = match auth::normalize_target(domain) {
        Ok(t) => t.key,
        Err(e) => {
            crate::diag!("{e}");
            return Err("invalid login domain".into());
        }
    };
    if !assume_yes {
        println!("Remove the stored login for {key}? [y/N] ");
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !answer.trim().eq_ignore_ascii_case("y")
        {
            println!("Cancelled.");
            return Ok(());
        }
    }
    let _lock = login_lock()?;
    let mut reg = AuthRegistry::load_checked()?;
    if reg.remove_checked(&key)? {
        println!("Removed {key} from the registry and the cookie vault.");
        println!("A running daemon picks the change up on its next tool call.");
    } else {
        println!("No stored login for {key}: nothing to remove.");
    }
    Ok(())
}

fn import(file: &str, domain: Option<&str>, probe: bool) -> Result<(), String> {
    let _lock = login_lock()?;
    let content =
        std::fs::read_to_string(file).map_err(|e| format!("cannot read cookies file: {e}"))?;
    let cookies = auth::parse_netscape(&content);
    let target = domain.map(auth::normalize_target).transpose()?;
    let (domains, probes) = auth::commit_login(
        &cookies,
        probe,
        target.as_ref().map(|t| t.key.as_str()),
        target.as_ref().and_then(|t| t.probe_port),
    )?;
    if domains.is_empty() {
        println!("No usable login cookies for the requested scope: nothing stored.");
        return Ok(());
    }
    println!("Imported:");
    let reg = AuthRegistry::load();
    for d in &domains {
        let names = reg
            .get(d)
            .map(|s| s.cookie_names.join(", "))
            .unwrap_or_default();
        println!("  {d}: {names}");
    }
    for (d, _, note) in &probes {
        println!("    probe {d} (unverified): {note}");
    }
    println!("Later fetches of these domains replay the stored cookies automatically.");
    Ok(())
}

// ── single-instance lock ─────────────────────────────────────

fn profile_in_use(profile: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        // Chromium leaves a hostname-pid SingletonLock symlink. A crashed
        // CLI can release our lock while its browser still owns this profile.
        let Some(pid) = std::fs::read_link(profile.join("SingletonLock"))
            .ok()
            .and_then(|p| {
                p.to_str()
                    .and_then(|s| s.rsplit('-').next())
                    .and_then(|s| s.parse::<i32>().ok())
            })
            .filter(|p| *p > 0)
        else {
            return false;
        };
        (unsafe { libc::kill(pid, 0) == 0 })
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let path = profile.join("lockfile");
        path.exists()
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(path)
                .is_err()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = profile;
        true
    }
}

fn login_lock() -> Result<AuthLock, String> {
    let base = crate::paths::cache_dir();
    std::fs::create_dir_all(&base).map_err(|e| format!("login cache: {e}"))?;
    AuthLock::acquire(&base.join("auth-profile.lock"))
}

/// OS-held profile lock. Process death releases it; age never permits takeover.
struct AuthLock {
    _file: std::fs::File,
}

impl AuthLock {
    fn acquire(path: &std::path::Path) -> Result<Self, String> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = options
            .open(path)
            .map_err(|e| format!("login profile lock: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("another login session is already running".into());
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `retrieve_ws` runs inside the tokio runtime driving `donsetch
    // login`. It used to build a `reqwest::blocking::Client` there,
    // which panics (and, with this crate's panic = "abort" release
    // profile, aborts the process) when built or dropped on a thread
    // that already has a runtime context.
    #[tokio::test]
    async fn retrieve_ws_times_out_without_panicking() {
        let err = retrieve_ws("http://127.0.0.1:1/json/version", 300)
            .await
            .expect_err("nothing listens on this port");
        assert!(err.contains("timeout"));
    }
    #[cfg(unix)]
    #[test]
    fn reliability_profile_lock_excludes_second_session_until_release() {
        let path = crate::paths::cache_dir().join("login-lock-regression");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lock = AuthLock::acquire(&path).unwrap();
        assert!(AuthLock::acquire(&path).is_err());
        drop(lock);
        assert!(
            AuthLock::acquire(&path).is_ok(),
            "a stale lock file must not block a new session"
        );
    }
    #[cfg(unix)]
    #[test]
    fn reliability_fresh_preserves_profile_owned_by_orphan_browser() {
        let profile = crate::paths::cache_dir().join("owned-browser-profile");
        std::fs::create_dir_all(&profile).unwrap();
        std::os::unix::fs::symlink(
            format!("fixture-{}", std::process::id()),
            profile.join("SingletonLock"),
        )
        .unwrap();
        assert!(profile_in_use(&profile));
        std::fs::remove_file(profile.join("SingletonLock")).unwrap();
        assert!(!profile_in_use(&profile));
    }
    #[cfg(windows)]
    #[test]
    fn reliability_windows_profile_lock_is_respected() {
        use std::os::windows::fs::OpenOptionsExt;
        let profile = crate::paths::cache_dir().join("owned-browser-profile");
        std::fs::create_dir_all(&profile).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(1)
            .open(profile.join("lockfile"))
            .unwrap();
        assert!(profile_in_use(&profile));
        drop(file);
        assert!(!profile_in_use(&profile));
    }
}
