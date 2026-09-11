use donsetch::cli;
use donsetch::config;
use donsetch::dev;
use donsetch::mcp;

/// Load the layered config, apply CLI overrides, install it process-wide.
/// Fatal on any config error: a bad knob must stop the daemon loudly.
fn load_and_install_config(args: &[String]) -> &'static config::DonsetchConfig {
    let mut c = match config::load() {
        Ok(loaded) => {
            for w in &loaded.warnings {
                eprintln!("[donsetch config] {w}");
            }
            loaded.config
        }
        Err(e) => {
            eprintln!("donsetch: {e}");
            std::process::exit(1);
        }
    };
    if args.iter().any(|a| a == "--http") {
        c.transport.kind = config::TransportKind::Http;
    }
    if let Some(h) = get_arg_value(args, "--host") {
        c.transport.host = h;
    }
    if let Some(p) = get_arg_value(args, "--port") {
        match p.parse::<u16>() {
            Ok(port) if port != 0 => c.transport.port = port,
            _ => {
                eprintln!("donsetch: invalid --port {p:?}");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = config::install(c) {
        eprintln!("donsetch: {e}");
        std::process::exit(1);
    }
    config::cfg()
}

/// `donsetch config show [--markdown] [--legacy]`
fn config_cli(args: &[String]) {
    let rest = &args[1..];
    if rest.iter().any(|a| a == "--legacy") {
        println!("{}", config::show_legacy_markdown());
        return;
    }
    match config::load() {
        Ok(loaded) => {
            for w in &loaded.warnings {
                eprintln!("[donsetch config] {w}");
            }
            if rest.iter().any(|a| a == "--markdown") {
                println!("{}", config::show_markdown());
            } else {
                print!("{}", config::show_text(&loaded));
            }
        }
        Err(e) => {
            eprintln!("donsetch: {e}");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    // CLI convention (rg, curl): a closed stdout pipe is normal,
    // not an error. Rust masks SIGPIPE by default, so every pipe-
    // tiled run (donsetch --help | head) turned the write into an
    // EPIPE panic with a full backtrace after head quit. Restoring
    // the default disposition makes the kill silent with exit 141,
    // exactly what the shell expects.
    //
    // NOT for `mcp`, though : SIG_DFL is process-wide, and in the
    // daemon it turns every EPIPE into an instant kill. That
    // bypasses stdio.rs's clean broken-transport shutdown (ghost
    // cleanup included; macOS Chrome has no pdeathsig to fall
    // back on), lets a BYOK plugin that exits without reading its
    // stdin take the whole daemon down through the request write,
    // and kills the supervisor exactly when its crashed child's
    // stdin comes back EPIPE : the hold-restart-replay contract
    // (supervisor.rs) depends on seeing that as an error.
    #[cfg(unix)]
    if cmd != "mcp" {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    // Every command hard-fails on a broken config (the lazy fallback
    // stays for library callers): a user must see the config error,
    // never a silent run on defaults. `config show` loads itself so
    // it can print the layers even when env values are bad; help and
    // version read no config at all.
    if !matches!(
        cmd,
        "config" | "help" | "--help" | "-h" | "version" | "-v" | "--version"
    ) {
        load_and_install_config(&args);
    }

    match cmd {
        // ── Agent tools (spec-driven, shared core, clap-parsed) ──
        "fetch" | "search" | "answer" | "crawl" | "memory" | "screenshot" => {
            let code = cli::tool::run(cmd, &args[2..]).await;
            std::process::exit(code as i32);
        }

        // ── Discovery ──
        "tools" => cli::tool::print_tools_json(),

        // ── Management ──
        "config" => {
            config_cli(&args);
            return;
        }
        "mcp" => {
            // Transport selection: the --http flag wins, then the
            // layered config ([transport] kind via TOML or env) for
            // launchers that can't pass flags, then stdio. Host/port:
            // flags beat the config.
            load_and_install_config(&args);
            #[cfg(feature = "http")]
            if config::cfg().transport.kind == config::TransportKind::Http {
                let host = config::cfg().transport.host.clone();
                let port = config::cfg().transport.port;
                eprintln!("[mcp] starting HTTP server on {}:{}", host, port);
                if let Err(e) = mcp::http::run(host, port).await {
                    eprintln!("mcp HTTP server: {e}");
                    std::process::exit(1);
                }
                return;
            }
            // Without the `http` cargo feature the checks above compile
            // out, so a requested HTTP transport would silently fall
            // through to stdio : fail loudly instead. (The linux-arm64
            // and macOS-x64 prebuilt binaries are core-only.)
            #[cfg(not(feature = "http"))]
            if config::cfg().transport.kind == config::TransportKind::Http {
                eprintln!(
                    "mcp: HTTP transport requested (--http / transport.kind = http), but this \
                     binary was built without the `http` cargo feature. Rebuild with \
                     --features http, or use a prebuilt binary that includes it."
                );
                std::process::exit(1);
            }
            if args.iter().any(|a| a == "--supervised") {
                // v3 crash-only design: `--supervised` spawns a child
                // daemon and proxies stdio; a panic-abort (release runs
                // panic=abort : one dead request would otherwise kill
                // the whole MCP session) restarts the child instead.
                // Persistent state (handles, history, profiles) reloads
                // from disk; the client sees a blip, not a death.
                eprintln!("[supervisor] donsetch mcp --supervised");
                if let Err(e) = mcp::supervisor::run() {
                    eprintln!("[supervisor] {e}");
                    std::process::exit(1);
                }
            } else if let Err(e) = mcp::server::run().await {
                eprintln!("mcp daemon: {e}");
                std::process::exit(1);
            }
        }
        "keys" => cli::keys::run(&args).await,

        "login" => cli::login::run(&args).await,
        "proxy" => cli::proxy::run(&args).await,
        "status" => cli::status::run().await,
        "adapters" => match args.get(2).map(String::as_str) {
            Some("--help" | "-h") => cli::adapters::help(),
            _ => cli::adapters::run(),
        },
        "stop" => cli::stop::run(),
        "doctor" | "--doctor" => cli::doctor::run().await,
        "update" | "-u" | "--update" => cli::update::run().await,
        "rollback" | "--rollback" => cli::rollback::run(),
        "version" | "-v" | "--version" => cli::version::run().await,

        // ── Dev/internal (hidden from --help) ──
        "dev" => dev::dispatch(&args[2..]).await,
        // Backward-compat: bare dev commands still work.
        "probe" | "fingerprint" | "resume-test" | "ghost" | "extract" => {
            dev::dispatch(&args[1..]).await;
        }

        "help" | "-h" | "--help" => {
            // Route `donsetch help <command>` to the command's help.
            if let Some(sub) = args.get(2).map(|s| s.as_str()) {
                route_help(sub).await;
            } else {
                cli::tool::print_top_help();
            }
        }
        _ => {
            eprintln!("donsetch: unknown command '{cmd}'\n");
            cli::tool::print_top_help();
            std::process::exit(1);
        }
    }
}

// ── Dev commands ─────────────────────────────────────────

/// Route `donsetch help <command>` to the command's own help.
/// Falls back to top-level help for unknown commands.
async fn route_help(cmd: &str) {
    match cmd {
        "fetch" | "search" | "answer" | "crawl" | "memory" | "screenshot" => {
            // Re-invoke with --help (clap handles the output).
            let help_args = vec!["--help".to_string()];
            let _ = cli::tool::run(cmd, &help_args).await;
        }
        "keys" => {
            cli::keys::run(&["donsetch".into(), "keys".into(), "help".into()]).await;
        }
        "adapters" => cli::adapters::help(),
        "proxy" => {
            // proxy::run is async, but print_help is sync.
            // Just call the help directly.
            println!("Usage: donsetch proxy <subcommand> [args]");
            println!();
            println!("Subcommands:");
            println!(
                "  add <url> [url...] [--no-check]  Add proxies (validated, optionally probed)"
            );
            println!("  remove <id> [id...]              Remove proxies by index or host:port");
            println!("  list                             Show all configured proxies");
            println!(
                "  check                            Probe all proxies (connectivity + exit IP)"
            );
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
            println!("Config: cache_dir/proxies.txt (one URL per line, # comments)");
            println!(
                "Env:   DONSEEK_PROXIES (comma-separated, overrides config for same host:port)"
            );
        }
        "status" => {
            println!("Usage: donsetch status");
            println!();
            println!("  Shows a quick overview: version, search config, proxies, cache, health.");
            println!("  No probes, no browser launch : fast.");
            println!("  For full diagnostics, run `donsetch doctor`.");
        }
        "config" => {
            println!("Usage: donsetch config show [--markdown] [--legacy]");
            println!();
            println!("  show            Print every knob, its current value and where");
            println!("                  it came from (default / env / file / legacy env).");
            println!("  --markdown      Print the full knob reference table as markdown");
            println!("                  (the source of truth for the README).");
            println!("  --legacy        Print the legacy env var -> key mapping.");
            println!();
            println!("  File layer: <config-dir>/donsetch/donsetch.toml, or the path in");
            println!("  DONSETCH_CONFIG. Skip the file layer with DONSETCH_NO_CONFIG_FILE=1.");
        }
        "doctor" => {
            println!("Usage: donsetch doctor");
            println!();
            println!("  13 health checks: binary, network, TLS, browser, Xvfb, ghost profile,");
            println!("  cache, permissions, PDFium, OCR models, rerank model, ghost state.");
            println!("  Auto-fixes what it can. Reports issues with instructions.");
        }
        "update" => {
            println!("Usage: donsetch update");
            println!();
            println!("  Checks for a new release and downloads the platform-correct binary");
            println!("  from GitHub Releases. Verifies SHA256, replaces in place, saves backup.");
        }
        "rollback" => {
            println!("Usage: donsetch rollback");
            println!();
            println!("  Reverts to the previous binary version (saved by `donsetch update`).");
            println!("  Can be run again to roll forward.");
        }
        "version" => {
            println!("Usage: donsetch version");
            println!();
            println!("  Shows build info and checks for updates.");
        }
        "mcp" => {
            println!("Usage: donsetch mcp [--http] [--host HOST] [--port PORT] [--supervised]");
            println!();
            println!("  Starts the MCP server (stdio or HTTP mode).");
            println!();
            println!("Options:");
            println!("  --http              Start HTTP server instead of stdio");
            println!("  --host HOST         Bind to this address (default: 127.0.0.1)");
            println!("  --port PORT         Listen on this port (default: 8765)");
            println!("  --supervised        Run with crash-recovery supervisor (stdio only)");
            println!();
            println!("Config: layered config = defaults < legacy env < donsetch.toml");
            println!("(<config-dir>/donsetch/donsetch.toml or DONSETCH_CONFIG=...) <");
            println!("DONSETCH_<SECTION>__<KEY>. `donsetch config show` prints the");
            println!("resolved table, `donsetch config show --markdown` the full knob");
            println!("reference, `donsetch config show --legacy` the old-name map.");
            println!();
            println!("Stdio mode (default): JSON-RPC over stdin/stdout");
            println!("HTTP mode: JSON-RPC POST at http://HOST:PORT/mcp (plus the");
            println!("            GET SSE stream and DELETE session end required by");
            println!("            streamable-HTTP clients)");
            println!();
            println!("Environment (flags win over the config):");
            println!("  DONSETCH_TRANSPORT__KIND=http Same as --http (stdio is the default)");
            println!("  DONSETCH_TRANSPORT__HOST=HOST Same as --host");
            println!("  DONSETCH_TRANSPORT__PORT=PORT Same as --port");
            println!("  DONSETCH_TRANSPORT__TOKEN=TOKEN");
            println!("                                Require Authorization: Bearer TOKEN on /mcp");
            println!("  DONSETCH_TRANSPORT__TIMEOUT_SECS=N  Per-request timeout (default 300)");
            println!("  DONSETCH_TRANSPORT__CORS=1    Allow cross-origin requests (default off)");
            println!();
            println!("Legacy names (DONSETCH_TRANSPORT, DONSETCH_HTTP_*, ...) still");
            println!("work and map to the keys above; `donsetch doctor` lists them.");
        }
        "login" => {
            println!("Usage: donsetch login [domain]");
            println!();
            println!("  Opens a real browser for you to sign into a site. Press Enter");
            println!("  when done: the session is stored (vault, 0600) and every later");
            println!("  fetch of that domain replays it, tier 1 and tier 2 alike.");
            println!();
            println!("  donsetch login --list      Show stored sessions (masked).");
            println!("  donsetch login --status D  Detail one domain.");
            println!("  donsetch login --logout D  Forget a domain.");
            println!("  donsetch login --import F  Import a Netscape cookies.txt export.");
            println!("  Credentials never enter donsetch: you type them into the browser.");
        }
        "tools" => {
            println!("Usage: donsetch tools");
            println!();
            println!("  Prints the tool schemas as JSON (same as MCP tools/list).");
        }
        _ => {
            cli::tool::print_top_help();
        }
    }
}

/// Helper function to extract argument value from args.
/// Returns None if the argument is not present or has no value.
fn get_arg_value(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == flag {
            // Check if there's a next argument that's not another flag
            if let Some(next) = iter.peek()
                && !next.starts_with("--")
            {
                return Some((**next).clone());
            }
            return None;
        }
        // Handle --flag=value format
        if let Some(rest) = arg.strip_prefix(flag)
            && let Some(rest) = rest.strip_prefix("=")
        {
            return Some(rest.to_string());
        }
    }
    None
}

// ── Dev commands ─────────────────────────────────────────────
