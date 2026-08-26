use donsetch::cli;
use donsetch::dev;
use donsetch::mcp;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        // ── Agent tools (spec-driven, shared core, clap-parsed) ──
        "fetch" | "search" | "crawl" => {
            let code = cli::tool::run(cmd, &args[2..]).await;
            std::process::exit(code as i32);
        }

        // ── Discovery ──
        "tools" => cli::tool::print_tools_json(),

        // ── Management ──
        "mcp" => {
            // Transport selection: the --http flag wins, then the
            // DONSETCH_TRANSPORT env (stdio|http) for launchers that
            // can't pass flags, then stdio. Host/port: flags, then
            // env, then defaults.
            let env_http = std::env::var("DONSETCH_TRANSPORT").as_deref() == Ok("http");
            if args.iter().any(|a| a == "--http") || env_http {
                let host = get_arg_value(&args, "--host")
                    .or_else(|| std::env::var("DONSETCH_HTTP_HOST").ok())
                    .unwrap_or("127.0.0.1".to_string());
                let port = get_arg_value(&args, "--port")
                    .or_else(|| std::env::var("DONSETCH_HTTP_PORT").ok())
                    .unwrap_or("8765".to_string())
                    .parse()
                    .unwrap_or(8765);
                eprintln!("[mcp] starting HTTP server on {}:{}", host, port);
                if let Err(e) = mcp::http::run(host, port).await {
                    eprintln!("mcp HTTP server: {e}");
                    std::process::exit(1);
                }
            } else if args.iter().any(|a| a == "--supervised") {
                // v3 crash-only design: `--supervised` spawns a child
                // daemon and proxies stdio; a panic-abort (release runs
                // panic=abort — one dead request would otherwise kill
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
        "keys" => cli::keys::run(&args),
        "proxy" => cli::proxy::run(&args).await,
        "status" => cli::status::run().await,
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
        "fetch" | "search" | "crawl" => {
            // Re-invoke with --help (clap handles the output).
            let help_args = vec!["--help".to_string()];
            let _ = cli::tool::run(cmd, &help_args).await;
        }
        "keys" => {
            cli::keys::run(&["donsetch".into(), "keys".into(), "help".into()]);
        }
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
            println!("  No probes, no browser launch — fast.");
            println!("  For full diagnostics, run `donsetch doctor`.");
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
            println!("Stdio mode (default): JSON-RPC over stdin/stdout");
            println!("HTTP mode: JSON-RPC POST at http://HOST:PORT/mcp (plus the");
            println!("            GET SSE stream and DELETE session end required by");
            println!("            streamable-HTTP clients)");
            println!();
            println!("Environment (flags win over env):");
            println!("  DONSETCH_TRANSPORT=http       Same as --http (stdio is the default)");
            println!("  DONSETCH_HTTP_HOST=HOST       Same as --host");
            println!("  DONSETCH_HTTP_PORT=PORT       Same as --port");
            println!("  DONSETCH_HTTP_TOKEN=TOKEN     Require Authorization: Bearer TOKEN on /mcp");
            println!("  DONSETCH_HTTP_TIMEOUT_SECS=N  Per-request timeout (default 300)");
            println!("  DONSETCH_HTTP_CORS=1          Allow cross-origin requests (default off)");
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
