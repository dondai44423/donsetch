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

use std::io::Write;

use tokio::io::AsyncBufReadExt;

use crate::auth::{self, AuthRegistry};
use crate::ghost::cache::CookieRecord;

pub async fn run(args: &[String]) {
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
                print_help();
                return;
            }
            other if other.starts_with("--") => {
                eprintln!("donsetch login: unknown flag {other}\n");
                print_help();
                return;
            }
            _ => {
                if site.is_none() {
                    site = Some(a.clone());
                } else {
                    eprintln!("donsetch login: unexpected argument '{a}'\n");
                    print_help();
                    return;
                }
            }
        }
    }

    let action = action_flags.first().copied().unwrap_or("interactive");
    match action {
        "list" => list_sites(),
        "status" => {
            let Some(s) = site else {
                eprintln!("donsetch login --status needs a domain");
                print_help();
                return;
            };
            print_status(&s);
        }
        "logout" => {
            let Some(s) = site else {
                eprintln!("donsetch login --logout needs a domain");
                print_help();
                return;
            };
            logout(&s, yes);
        }
        "import" => {
            let Some(f) = import_file else {
                eprintln!("donsetch login --import needs a cookies.txt file");
                print_help();
                return;
            };
            import(&f, site.as_deref(), !no_probe);
        }
        _ => {
            if let Err(e) = interactive(site.as_deref(), fresh, !no_probe).await {
                eprintln!("donsetch login: {e}");
            }
        }
    }
}

fn print_help() {
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
    println!("  --no-probe    Skip the post-login verification probe.");
    println!("  --yes | -y    Confirm --logout without prompting.");
    println!();
    println!("Credentials are typed into the browser only. donsetch never sees");
    println!("your password: no keystroke capture, no screenshots, no CDP before");
    println!("you press Enter. Stored cookie values live in the same 0600 vault");
    println!("the fetch engine already uses; the registry holds metadata only.");
}

// ── interactive flow ─────────────────────────────────────────

async fn interactive(site: Option<&str>, fresh: bool, probe: bool) -> Result<(), String> {
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
    if std::env::var_os("DONSETCH_LOGIN_FORCE").is_none() && !display_available() {
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
    if fresh && profile_dir.exists() {
        std::fs::remove_dir_all(&profile_dir).map_err(|e| format!("--fresh wipe: {e}"))?;
    }
    std::fs::create_dir_all(&profile_dir).map_err(|e| format!("auth profile: {e}"))?;

    let _lock = AuthLock::acquire(&lock_path)?;

    // Ephemeral debug port.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("port: {e}"))?;
        l.local_addr()
            .map_err(|e| format!("port addr: {e}"))?
            .port()
    };

    let log_file = std::fs::File::create(&browser_log).map_err(|e| format!("browser log: {e}"))?;
    let mut cmd = tokio::process::Command::new(&resolved.path);
    cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-allow-origins=*")
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

    // Baseline cookies (multi-site delta only).
    let cdp = crate::ghost::cdp::Cdp::connect(&ws)
        .await
        .map_err(|e| format!("CDP attach: {e}"))?;
    let baseline_res = cdp
        .call(None, "Storage.getCookies", serde_json::json!({}))
        .await
        .map(|r| crate::ghost::Ghost::parse_cdp_cookies(&r))
        .unwrap_or_default();
    println!("Log in now. Come back here and press Enter to save the session.");
    if site.is_none() {
        println!("(multi-site mode: every NEW session you create gets saved)");
    }
    println!("Ctrl+C discards everything and closes the browser.");

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
            if r.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err("stdin closed: nothing was stored".into());
            }
        }
    }

    let final_res = cdp
        .call(None, "Storage.getCookies", serde_json::json!({}))
        .await
        .map(|r| crate::ghost::Ghost::parse_cdp_cookies(&r))
        .unwrap_or_default();

    // Close the browser cleanly (goodbye, cookies flushed).
    let _ = cdp.call(None, "Browser.close", serde_json::json!({})).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(6), child.wait()).await;

    // Filter + store.
    let kept: Vec<CookieRecord> = match key.as_deref() {
        Some(d) => final_res
            .into_iter()
            .filter(|c| auth::cookie_belongs_to(d, c.domain.trim_start_matches('.')))
            .collect(),
        None => {
            let before: std::collections::HashSet<(String, String)> = baseline_res
                .iter()
                .map(|c| (c.domain.clone(), c.name.clone()))
                .collect();
            final_res
                .into_iter()
                .filter(|c| !before.contains(&(c.domain.clone(), c.name.clone())))
                .collect()
        }
    };

    if kept.is_empty() {
        println!("No new session cookies found: nothing stored.");
        return Ok(());
    }
    let worth: Vec<CookieRecord> = kept
        .iter()
        .filter(|c| crate::ghost::cache::is_session_worthy(c))
        .cloned()
        .collect();
    if worth.is_empty() {
        println!(
            "The browser has new cookies, but none look like session cookies \
             (all tracking/preference noise). Nothing stored."
        );
        return Ok(());
    }

    let (domains, probes) = auth::commit_login(&worth, probe, key.as_deref(), probe_port);
    println!("\nSaved:");
    for d in &domains {
        println!("  {d}");
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
            eprintln!("{e}");
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
            let vaulted = crate::ghost::cache::load_session_cookies()
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
                    None => "not probed",
                }
            );
            println!("Last:     {}", s.last_login);
            if !s.cookie_names.is_empty() {
                println!("Names:    {}", s.cookie_names.join(", "));
            }
        }
    }
}

fn logout(domain: &str, assume_yes: bool) {
    let key = match auth::normalize_target(domain) {
        Ok(t) => t.key,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    if !assume_yes {
        println!("Remove the stored login for {key}? [y/N] ");
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !answer.trim().eq_ignore_ascii_case("y")
        {
            println!("Cancelled.");
            return;
        }
    }
    let mut reg = AuthRegistry::load();
    if reg.remove(&key) {
        println!("Removed {key} from the registry and the cookie vault.");
        println!("A running daemon picks the change up on its next tool call.");
    } else {
        println!("No stored login for {key}: nothing to remove.");
    }
}

fn import(file: &str, domain: Option<&str>, probe: bool) {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {file}: {e}");
            return;
        }
    };
    let mut cookies = auth::parse_netscape(&content);
    let mut probe_key = None;
    let mut probe_port = None;
    if let Some(d) = domain {
        let target = match auth::normalize_target(d) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{e}");
                return;
            }
        };
        cookies.retain(|c| auth::cookie_belongs_to(&target.key, c.domain.trim_start_matches('.')));
        probe_key = Some(target.key);
        probe_port = target.probe_port;
    }
    if cookies.is_empty() {
        println!("No usable cookies in {file}.");
        return;
    }
    let (domains, probes) = auth::commit_login(&cookies, probe, probe_key.as_deref(), probe_port);
    println!("Imported:");
    for d in &domains {
        println!("  {d}");
    }
    for (d, ok, note) in &probes {
        println!(
            "    probe {d}{}: {note}",
            if *ok { " ✓" } else { " (unverified)" }
        );
    }
    println!("Later fetches of these domains replay the session automatically.");
}

// ── single-instance lock ─────────────────────────────────────

/// Lockfile guard for the auth flow: one login session at a time.
/// Stale (>10min) locks are reclaimed; content records the pid.
struct AuthLock {
    path: std::path::PathBuf,
}

impl AuthLock {
    fn acquire(path: &std::path::Path) -> Result<Self, String> {
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(AuthLock {
                        path: path.to_path_buf(),
                    });
                }
                Err(_) => {
                    // Reclaim stale locks by age; otherwise fail so two
                    // login browsers never fight one profile.
                    let stale = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .map(|age| age > std::time::Duration::from_secs(600))
                        .unwrap_or(false);
                    if stale {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    return Err(
                        "another login session is already running (auth-profile.lock)".into(),
                    );
                }
            }
        }
        Err("could not acquire the login lock".into())
    }
}

impl Drop for AuthLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
}
