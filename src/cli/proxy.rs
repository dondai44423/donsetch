//! `donsetch proxy` : manage proxy configuration.
//!
//! Subcommands:
//!   add <url> [url...] [--no-check]  Add proxies (validated, optionally probed)
//!   remove <id> [id...]              Remove proxies by host:port or URL
//!   list                             Show all configured proxies
//!   check                            Probe all proxies (connectivity + exit IP)
//!   clear                            Remove all proxies
//!   test <url>                       Test a single proxy without adding
//!   import <file>                    Import proxies from file (one URL per line)
//!   export [file]                    Export to file (default: stdout)
//!
//! Config: cache_dir/proxies.txt (one URL per line, # comments)
//! Env: DONSEEK_PROXIES (comma-separated, overrides config for same host:port)

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::DISPLAY_NAME;
use crate::cli;
use crate::transport::proxy::{self, Proxy, ProxyScheme};

const PROBE_HOST: &str = "api.ipify.org";
const PROBE_PORT: u16 = 80;
const SLOW_THRESHOLD: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

struct ProbeResult {
    alive: bool,
    exit_ip: Option<String>,
    latency: Duration,
    error: Option<String>,
}

// ── Dispatch ──────────────────────────────────────────────────

pub async fn run(args: &[String]) {
    cli::init();
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("help");
    match sub {
        "add" => cmd_add(&args[3..]).await,
        "remove" | "rm" => cmd_remove(&args[3..]).await,
        "list" | "ls" => cmd_list().await,
        "check" => cmd_check().await,
        "clear" => cmd_clear().await,
        "test" => cmd_test(&args[3..]).await,
        "import" => cmd_import(&args[3..]).await,
        "export" => cmd_export(&args[3..]).await,
        _ => print_help(),
    }
}

// ── Subcommands ──────────────────────────────────────────────

async fn cmd_add(args: &[String]) {
    let mut check = true;
    let mut urls: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--no-check" | "--fast" => check = false,
            _ => urls.push(arg),
        }
    }

    if urls.is_empty() {
        eprintln!("Usage: donsetch proxy add <url> [url...] [--no-check]");
        eprintln!("  Adds one or more proxies. Validated and probed by default.");
        eprintln!("  --no-check: skip connectivity probing for speed.");
        std::process::exit(1);
    }

    cli::print_title(&format!("{DISPLAY_NAME} Proxy Add"));
    println!();

    let existing = proxy::load_config();

    // Parse all URLs, classify each as new, duplicate, or invalid.
    enum Status {
        New(Proxy),
        Duplicate(Proxy),
        Invalid(String, String), // (raw_url, error_msg)
    }

    let mut entries: Vec<Status> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();

    for url in &urls {
        match Proxy::parse(url) {
            Ok(p) => {
                let id = p.id();
                if existing.iter().any(|e| e.id() == id) || seen_ids.contains(&id) {
                    entries.push(Status::Duplicate(p));
                } else {
                    seen_ids.push(id);
                    entries.push(Status::New(p));
                }
            }
            Err(e) => {
                entries.push(Status::Invalid(url.to_string(), e.to_string()));
            }
        }
    }

    // Probe new proxies if checking.
    let new_proxies: Vec<Proxy> = entries
        .iter()
        .filter_map(|s| match s {
            Status::New(p) => Some(p.clone()),
            _ => None,
        })
        .collect();

    let probe_results = if check && !new_proxies.is_empty() {
        let spinner = cli::Spinner::new(&format!("Probing {} proxies...", new_proxies.len()));
        let results = probe_all(&new_proxies).await;
        spinner.stop();
        results
    } else {
        Vec::new()
    };

    // Print results, collect for saving.
    let mut added = 0u32;
    let mut skipped = 0u32;
    let mut to_save: Vec<Proxy> = Vec::new();
    let mut probe_idx = 0usize;

    for status in &entries {
        match status {
            Status::New(p) => {
                to_save.push(p.clone());
                added += 1;
                let detail = if check && probe_idx < probe_results.len() {
                    let r = &probe_results[probe_idx];
                    probe_idx += 1;
                    format_probe_detail(r)
                } else {
                    String::new()
                };
                if detail.is_empty() {
                    println!(
                        "  {} {:<7} {:<30} added",
                        cli::icon_pass(),
                        scheme_str(p),
                        p.id()
                    );
                } else {
                    println!(
                        "  {} {:<7} {:<30} added  {}",
                        cli::icon_pass(),
                        scheme_str(p),
                        p.id(),
                        detail
                    );
                }
            }
            Status::Duplicate(p) => {
                skipped += 1;
                println!(
                    "  {} {:<7} {:<30} duplicate, skipped",
                    cli::icon_fail(),
                    scheme_str(p),
                    p.id()
                );
            }
            Status::Invalid(url, e) => {
                skipped += 1;
                println!(
                    "  {} {}",
                    cli::icon_fail(),
                    cli::dim(&format!("{url}  invalid: {e}"))
                );
            }
        }
    }

    // Save.
    if !to_save.is_empty() {
        let mut merged = existing;
        merged.extend(to_save);
        if let Err(e) = proxy::save_config(&merged) {
            eprintln!("  {} Failed to save config: {e}", cli::red("error:"));
            std::process::exit(1);
        }
    }

    println!();
    println!("  {} added, {} skipped", added, skipped);
    cli::print_footer();
}

async fn cmd_remove(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: donsetch proxy remove <id> [id...]");
        eprintln!("  Remove by index (1, 2, ...) from `proxy list` or by host:port / URL.");
        std::process::exit(1);
    }

    cli::print_title(&format!("{DISPLAY_NAME} Proxy Remove"));
    println!();

    let mut proxies = proxy::load_config();
    let mut removed = 0u32;
    let mut not_found = 0u32;

    // Collect indices to remove. Parse each arg as either a
    // 1-based index (from `proxy list`) or a host:port/URL.
    // Collect indices first so shifting doesn't bite us.
    let mut to_remove: Vec<usize> = Vec::new();
    let mut not_found_args: Vec<String> = Vec::new();

    for arg in args {
        let pos = if let Ok(n) = arg.parse::<usize>()
            && n >= 1
            && n <= proxies.len()
            && !to_remove.contains(&(n - 1))
        {
            Some(n - 1)
        } else if let Ok(n) = arg.parse::<usize>()
            && n >= 1
        {
            // Index out of range : report as not found.
            None
        } else {
            let id = normalize_id(arg);
            proxies.iter().position(|p| p.id() == id)
        };

        match pos {
            Some(idx) if !to_remove.contains(&idx) => {
                to_remove.push(idx);
            }
            _ => {
                not_found_args.push(arg.clone());
                not_found += 1;
            }
        }
    }

    // Remove in reverse order so indices don't shift.
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    for idx in &to_remove {
        let p = proxies.remove(*idx);
        println!("  {} {:<30} removed", cli::icon_pass(), p.id());
        removed += 1;
    }
    for arg in &not_found_args {
        println!("  {} {:<30} not found", cli::icon_fail(), arg);
    }

    if removed > 0
        && let Err(e) = proxy::save_config(&proxies)
    {
        eprintln!("  {} Failed to save config: {e}", cli::red("error:"));
        std::process::exit(1);
    }

    println!();
    println!("  {} removed, {} not found", removed, not_found);
    cli::print_footer();
}

async fn cmd_list() {
    cli::print_title(&format!("{DISPLAY_NAME} Proxy Configuration"));
    println!();

    let proxies = proxy::load_config();
    if proxies.is_empty() {
        println!("  No proxies configured.");
        println!("  Use `donsetch proxy add <url>` to add one.");
        println!();
        cli::print_footer();
        return;
    }

    for (i, p) in proxies.iter().enumerate() {
        let auth = if p.user.is_empty() {
            cli::dim("no auth")
        } else {
            cli::green("auth")
        };
        println!(
            "  {:>3}  {:<7}  {:<30}  {}",
            i + 1,
            scheme_str(p),
            p.id(),
            auth
        );
    }

    println!();
    let n = proxies.len();
    let word = if n == 1 { "proxy" } else { "proxies" };
    println!("  {} {} configured", n, word);
    println!(
        "  {}",
        cli::dim(&format!("config: {}", proxy::config_path().display()))
    );
    cli::print_footer();
}

async fn cmd_check() {
    cli::print_title(&format!("{DISPLAY_NAME} Proxy Check"));
    println!();

    let proxies = proxy::load_config();
    if proxies.is_empty() {
        println!("  No proxies configured.");
        println!("  Use `donsetch proxy add <url>` to add one.");
        println!();
        cli::print_footer();
        return;
    }

    let spinner = cli::Spinner::new(&format!("Probing {} proxies...", proxies.len()));
    let results = probe_all(&proxies).await;
    spinner.stop();

    let mut alive = 0u32;
    let mut dead = 0u32;
    let mut slow = 0u32;

    for (i, p) in proxies.iter().enumerate() {
        let r = &results[i];
        let detail = format_probe_detail(r);
        if r.alive {
            alive += 1;
            if r.latency > SLOW_THRESHOLD {
                slow += 1;
                println!(
                    "  {} {:>3}  {:<7}  {:<30}  {}",
                    cli::icon_warn(),
                    i + 1,
                    scheme_str(p),
                    p.id(),
                    detail
                );
            } else {
                println!(
                    "  {} {:>3}  {:<7}  {:<30}  {}",
                    cli::icon_pass(),
                    i + 1,
                    scheme_str(p),
                    p.id(),
                    detail
                );
            }
        } else {
            dead += 1;
            println!(
                "  {} {:>3}  {:<7}  {:<30}  {}",
                cli::icon_fail(),
                i + 1,
                scheme_str(p),
                p.id(),
                detail
            );
        }
    }

    println!();
    let mut parts = Vec::new();
    if alive > 0 {
        parts.push(format!("{} alive", cli::green(&alive.to_string())));
    }
    if dead > 0 {
        parts.push(format!("{} dead", cli::red(&dead.to_string())));
    }
    if slow > 0 {
        parts.push(format!("{} slow", cli::yellow(&slow.to_string())));
    }
    parts.push(format!("{} total", proxies.len()));
    println!("  {}", parts.join(", "));
    cli::print_footer();

    if dead > 0 {
        std::process::exit(1);
    }
}

async fn cmd_clear() {
    cli::print_title(&format!("{DISPLAY_NAME} Proxy Clear"));
    println!();

    let existing = proxy::load_config();
    let count = existing.len();

    if count == 0 {
        println!("  No proxies to clear.");
        println!();
        cli::print_footer();
        return;
    }

    if let Err(e) = proxy::save_config(&[]) {
        eprintln!("  {} Failed to clear config: {e}", cli::red("error:"));
        std::process::exit(1);
    }

    let word = if count == 1 { "proxy" } else { "proxies" };
    println!("  {} Cleared {} {}", cli::icon_pass(), count, word);
    println!();
    cli::print_footer();
}

async fn cmd_test(args: &[String]) {
    let url = match args.first() {
        Some(u) => u.as_str(),
        None => {
            eprintln!("Usage: donsetch proxy test <url>");
            eprintln!("  Tests a single proxy without adding it to the config.");
            std::process::exit(1);
        }
    };

    let p = match Proxy::parse(url) {
        Ok(p) => p,
        Err(e) => {
            cli::print_title(&format!("{DISPLAY_NAME} Proxy Test"));
            println!();
            println!("  {} Invalid proxy URL: {}", cli::icon_fail(), e);
            println!();
            cli::print_footer();
            std::process::exit(1);
        }
    };

    cli::print_title(&format!("{DISPLAY_NAME} Proxy Test"));
    println!();

    let spinner = cli::Spinner::new("Probing proxy...");
    let r = probe_proxy(&p).await;
    spinner.stop();

    let detail = format_probe_detail(&r);
    if r.alive {
        println!(
            "  {} {:<7}  {:<30}  {}",
            cli::icon_pass(),
            scheme_str(&p),
            p.id(),
            detail
        );
        println!();
        println!("  Status: {}", cli::green("alive"));
    } else {
        println!(
            "  {} {:<7}  {:<30}  {}",
            cli::icon_fail(),
            scheme_str(&p),
            p.id(),
            detail
        );
        println!();
        println!("  Status: {}", cli::red("not reachable"));
    }
    cli::print_footer();

    if !r.alive {
        std::process::exit(1);
    }
}

async fn cmd_import(args: &[String]) {
    let path = match args.first() {
        Some(p) => p.as_str(),
        None => {
            eprintln!("Usage: donsetch proxy import <file>");
            eprintln!("  Imports proxies from a text file (one URL per line, # comments).");
            std::process::exit(1);
        }
    };

    cli::print_title(&format!("{DISPLAY_NAME} Proxy Import"));
    println!();

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  {} Cannot read file: {e}", cli::icon_fail());
            std::process::exit(1);
        }
    };

    let mut existing = proxy::load_config();
    let mut added = 0u32;
    let mut dup = 0u32;
    let mut invalid = 0u32;

    for line in content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        match Proxy::parse(line) {
            Ok(p) => {
                if existing.iter().any(|e| e.id() == p.id()) {
                    dup += 1;
                } else {
                    existing.push(p);
                    added += 1;
                }
            }
            Err(_) => invalid += 1,
        }
    }

    if added > 0
        && let Err(e) = proxy::save_config(&existing)
    {
        eprintln!("  {} Failed to save config: {e}", cli::red("error:"));
        std::process::exit(1);
    }

    let word = if added == 1 { "proxy" } else { "proxies" };
    println!(
        "  {} {} {} imported from {}",
        cli::icon_pass(),
        added,
        word,
        path
    );
    if dup > 0 {
        println!("  {} {} duplicate (skipped)", cli::icon_warn(), dup);
    }
    if invalid > 0 {
        println!("  {} {} invalid (skipped)", cli::icon_fail(), invalid);
    }
    println!();
    let total = added + dup + invalid;
    println!(
        "  {} total, {} added, {} skipped",
        total,
        added,
        dup + invalid
    );
    cli::print_footer();
}

async fn cmd_export(args: &[String]) {
    let proxies = proxy::load_config();

    match args.first() {
        Some(path) => {
            cli::print_title(&format!("{DISPLAY_NAME} Proxy Export"));
            println!();
            let content: String = proxies
                .iter()
                .map(|p| p.to_url())
                .collect::<Vec<_>>()
                .join("\n");
            let content = if content.is_empty() {
                String::new()
            } else {
                format!("{content}\n")
            };
            match std::fs::write(path, &content) {
                Ok(()) => {
                    let n = proxies.len();
                    let word = if n == 1 { "proxy" } else { "proxies" };
                    println!("  {} {} {} exported to {}", cli::icon_pass(), n, word, path);
                }
                Err(e) => {
                    eprintln!("  {} Cannot write file: {e}", cli::icon_fail());
                    std::process::exit(1);
                }
            }
            println!();
            cli::print_footer();
        }
        None => {
            // Print to stdout (machine-readable, no TUI).
            for p in &proxies {
                println!("{}", p.to_url());
            }
        }
    }
}

// ── Probe ────────────────────────────────────────────────────

/// Connect through the proxy to api.ipify.org:80 via HTTP/1.0.
/// No TLS : we're testing the tunnel, not the TLS stack. The
/// response body is the exit IP address.
async fn probe_proxy(px: &Proxy) -> ProbeResult {
    let t0 = Instant::now();

    // Step 1: tunnel through the proxy to the probe endpoint.
    let connect = px.connect(PROBE_HOST, PROBE_PORT);
    let mut stream = match tokio::time::timeout(PROBE_TIMEOUT, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return ProbeResult {
                alive: false,
                exit_ip: None,
                latency: t0.elapsed(),
                error: Some(e.to_string()),
            };
        }
        Err(_) => {
            return ProbeResult {
                alive: false,
                exit_ip: None,
                latency: t0.elapsed(),
                error: Some("connection timeout".into()),
            };
        }
    };

    // Step 2: HTTP/1.0 GET (server closes after response).
    let req = format!("GET / HTTP/1.0\r\nHost: {PROBE_HOST}\r\nConnection: close\r\n\r\n");
    let write_fut = stream.write_all(req.as_bytes());
    if tokio::time::timeout(Duration::from_secs(5), write_fut)
        .await
        .is_err()
    {
        return ProbeResult {
            alive: false,
            exit_ip: None,
            latency: t0.elapsed(),
            error: Some("write timeout".into()),
        };
    }

    // Step 3: read response until EOF (HTTP/1.0 closes connection).
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let read_loop = async {
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() > 8192 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), read_loop).await;

    let latency = t0.elapsed();
    let body = String::from_utf8_lossy(&buf);

    // Body is after \r\n\r\n. api.ipify.org returns just the IP.
    let ip_str = body.split("\r\n\r\n").nth(1).unwrap_or("").trim();

    if !ip_str.is_empty() && ip_str.parse::<std::net::IpAddr>().is_ok() {
        ProbeResult {
            alive: true,
            exit_ip: Some(ip_str.to_string()),
            latency,
            error: None,
        }
    } else {
        ProbeResult {
            alive: false,
            exit_ip: None,
            latency,
            error: Some(if ip_str.is_empty() {
                "no exit IP returned".into()
            } else {
                format!("invalid response: {}", &ip_str[..ip_str.len().min(80)])
            }),
        }
    }
}

/// Probe all proxies in parallel.
async fn probe_all(proxies: &[Proxy]) -> Vec<ProbeResult> {
    let futures: Vec<_> = proxies.iter().map(probe_proxy).collect();
    futures_util::future::join_all(futures).await
}

// ── Helpers ──────────────────────────────────────────────────

fn scheme_str(p: &Proxy) -> &'static str {
    match p.scheme {
        ProxyScheme::Http => "http",
        ProxyScheme::Socks5 => "socks5",
    }
}

/// Normalize a user-provided arg to host:port for matching.
/// Accepts both "host:port" and full proxy URLs.
fn normalize_id(arg: &str) -> String {
    if let Ok(p) = Proxy::parse(arg) {
        p.id()
    } else {
        arg.to_string()
    }
}

fn format_latency(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Build the detail column for a probe result.
/// Alive: `23ms  exit: 1.2.3.4` (dim, with yellow "slow" tag if slow)
/// Dead:  `dead  connection refused` (red + dim)
fn format_probe_detail(r: &ProbeResult) -> String {
    if r.alive {
        let lat = format_latency(r.latency);
        let ip = r.exit_ip.as_deref().unwrap_or("?");
        if r.latency > SLOW_THRESHOLD {
            format!(
                "{}  exit: {} {}",
                cli::dim(&lat),
                cli::dim(ip),
                cli::yellow("(slow)")
            )
        } else {
            format!("{}  exit: {}", cli::dim(&lat), cli::dim(ip))
        }
    } else {
        let err = r.error.as_deref().unwrap_or("unknown");
        format!("{} {}", cli::red("dead"), cli::dim(err))
    }
}

fn print_help() {
    println!("Usage: donsetch proxy <subcommand> [args]");
    println!();
    println!("Subcommands:");
    println!("  add <url> [url...] [--no-check]  Add proxies (validated, optionally probed)");
    println!("  remove <id> [id...]              Remove by index (1, 2, ...) or host:port / URL");
    println!("  list                             Show all configured proxies");
    println!("  check                            Probe all proxies (connectivity + exit IP)");
    println!("  clear                            Remove all proxies");
    println!("  test <url>                       Test a proxy without adding it");
    println!("  import <file>                    Import from file (one URL per line)");
    println!("  export [file]                    Export to file (default: stdout)");
    println!();
    println!("Proxy URL format:");
    println!("  socks5://user:pass@host:port     SOCKS5 with auth (remote DNS, no leak)");
    println!("  socks5://host:port               SOCKS5 without auth");
    println!("  http://user:pass@host:port       HTTP CONNECT with auth");
    println!("  http://host:port                 HTTP CONNECT without auth");
    println!("  user:pass@host:port              Bare = HTTP CONNECT (backward compat)");
    println!("  host:port                        No auth, HTTP CONNECT");
    println!();
    println!("Examples:");
    println!("  donsetch proxy add socks5://user:pass@1.2.3.4:1080");
    println!("  donsetch proxy list");
    println!("  donsetch proxy remove 1          # remove first proxy from list");
    println!("  donsetch proxy remove 1.2.3.4:1080");
    println!("  donsetch proxy check");
    println!();
    println!("Config: cache_dir/proxies.txt (one URL per line, # comments)");
    println!("Env:   DONSEEK_PROXIES (comma-separated, overrides config for same host:port)");
}
