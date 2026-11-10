use donsetch::cli;
use donsetch::config;
use donsetch::dev;
use donsetch::mcp;

/// Load the layered config, apply CLI overrides, install it process-wide.
/// Fatal on any config error: a bad knob must stop the daemon loudly.
fn load_and_install_config(args: &[String]) -> &'static config::DonsetchConfig {
    let mut loaded = match config::load() {
        Ok(loaded) => {
            for w in &loaded.warnings {
                donsetch::diag!("[donsetch config] {w}");
            }
            loaded
        }
        Err(e) => {
            donsetch::diag!("donsetch: {e}");
            std::process::exit(1);
        }
    };
    let c = &mut loaded.config;
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
                donsetch::diag!("donsetch: invalid --port {p:?}");
                std::process::exit(1);
            }
        }
    }
    // `--ignore-rules` is the same switch as `[rules] mode = "off"`, so
    // the two cannot drift apart. Only the
    // commands that declare the flag honor it; clap rejects it anywhere
    // else before any work runs.
    if args
        .get(1)
        .is_some_and(|cmd| donsetch::spec::IGNORE_RULES_COMMANDS.contains(&cmd.as_str()))
        && args.iter().skip(2).any(|a| a == "--ignore-rules")
    {
        c.rules.mode = donsetch::rules::RulesMode::Off;
    }
    if let Err(e) = config::install_loaded(loaded) {
        donsetch::diag!("donsetch: {e}");
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
                donsetch::diag!("[donsetch config] {w}");
            }
            if rest.iter().any(|a| a == "--markdown") {
                println!("{}", config::show_markdown());
            } else {
                print!("{}", config::show_text(&loaded));
            }
        }
        Err(e) => {
            donsetch::diag!("donsetch: {e}");
            std::process::exit(1);
        }
    }
}

/// Stack size for the runtime's worker threads.
///
/// Tokio gives workers 2 MiB by default, and the MCP server runs every
/// tool call on one of them. The fetch path needs more than that in an
/// unoptimized build: measured on the `fast` profile, 2 MiB and 2.5 MiB
/// both overflowed and the first size that passed was 2.75 MiB. An
/// overflow is an abort with no error envelope, which is the worst
/// failure a caller can get: `donsetch mcp` killed the whole session on
/// the first fetch while the same fetch through the CLI was fine, because
/// the CLI runs on the 8 MiB main thread. Workers get what the main thread
/// already has, so the same work cannot be fine on one path and fatal on
/// the other. Reserved, not committed: the pages are only touched as they
/// are used.
const WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Say once, on stderr, that a v4 install is now running the v5 fetch engine.
///
/// The signal is the v4 state file: it carries the session-vault and tier-1
/// fields this binary no longer writes. A fresh v5 install never has them, so
/// this cannot fire for one. The stamp makes it once per cache directory.
fn announce_upgrade_once(cmd: &str, args: &[String]) {
    use std::io::IsTerminal;
    if cmd == "mcp" || args.iter().any(|a| a == "--json") || !std::io::stderr().is_terminal() {
        return;
    }
    let cache = donsetch::paths::cache_dir();
    let stamp = cache.join("upgrade-notice");
    if stamp.exists() {
        return;
    }
    let Ok(bytes) = std::fs::read(cache.join("ghost-state.json")) else {
        return;
    };
    if !has_v4_state(&bytes) {
        return;
    }
    // diag!, not eprintln!: a closed stderr must not abort the process
    // (src/diag.rs, and the crate denies print_stderr).
    donsetch::diag!();
    donsetch::diag!("  DonSeTch v5 is a different fetch engine, not a v4 patch.");
    donsetch::diag!(
        "    - The fetch path embeds V8 and carries identity and state; no browser runs it."
    );
    donsetch::diag!(
        "    - `donsetch login` and the session vault are gone. A login wall is reported,"
    );
    donsetch::diag!(
        "      never crossed; authenticated browsing belongs to a browser driver (bladebro)."
    );
    donsetch::diag!("    - v4 state is left in place and ignored by the new engine.");
    donsetch::diag!(
        "  Run `donsetch doctor` to see what this install uses, `donsetch help` for commands."
    );
    donsetch::diag!();
    let _ = std::fs::create_dir_all(&cache);
    let _ = std::fs::write(&stamp, env!("CARGO_PKG_VERSION"));
}

/// Does this state file carry the v4 session vault? The fields are never
/// written by this binary, so their presence dates the file to v4.
fn has_v4_state(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(65_536)];
    let text = String::from_utf8_lossy(head);
    text.contains("\"vault_epoch\"")
        || text.contains("\"session_cookies\"")
        || text.contains("\"tier1_cookies\"")
}

/// Every top-level command, for the suggestion on a typo.
const KNOWN_COMMANDS: &[&str] = &[
    "fetch",
    "search",
    "crawl",
    "screenshot",
    "tools",
    "mcp",
    "keys",
    "proxy",
    "rules",
    "status",
    "adapters",
    "stop",
    "doctor",
    "update",
    "rollback",
    "version",
    "config",
    "search-bench",
    "help",
];

fn main() {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            donsetch::diag!("donsetch: cannot start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    rt.block_on(run());
}

async fn run() {
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
    // bypasses stdio.rs's clean broken-transport shutdown (browser-child
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

    // `--help`/`-h` right after a command name always means HELP, never a
    // run. The spec-driven tools plus keys/proxy/adapters carry their
    // own help; every other management command used to EXECUTE on the flag
    // (`status --help` printed the status, `doctor --help` ran the whole
    // health check, `update --help` checked the network, `rollback --help`
    // touched the install, `mcp --help` opened a daemon on stdin). Route
    // them to the same help text `donsetch help <cmd>` prints, before any
    // dispatch or config load.
    if matches!(args.get(2).map(String::as_str), Some("--help" | "-h"))
        && !matches!(
            cmd,
            "fetch" | "search" | "crawl" | "screenshot" | "keys" | "proxy" | "adapters"
        )
    {
        route_help(cmd).await;
        return;
    }

    // Every command hard-fails on a broken config (the lazy fallback
    // stays for library callers): a user must see the config error,
    // never a silent run on defaults. `config show` loads itself so
    // it can print the layers even when env values are bad; help and
    // version read no config at all.
    if !matches!(
        cmd,
        "config" | "help" | "--help" | "-h" | "version" | "-v" | "-V" | "--version"
    ) {
        load_and_install_config(&args);
    }

    // A v4 install updated in place lands on a different fetch engine, and
    // the v4 binary that performed the update could not warn (it shipped
    // before this check existed). The first interactive run after the
    // update says what changed, once. stderr only: stdout belongs to the
    // command's own result, and `mcp`/`--json` must stay machine-clean.
    announce_upgrade_once(cmd, &args);

    match cmd {
        // ── Agent tools (spec-driven, shared core, clap-parsed) ──
        // `search --sources` / `search --doctor`: source health, not a search.
        "search" if donsetch::search::doctor::wants(&args[2..]) => {
            let code = donsetch::search::doctor::run(&args[2..]).await;
            std::process::exit(code);
        }
        // `search --explain <query>`: each result's features and weights.
        "search" if donsetch::search::explain::wants(&args[2..]) => {
            let code = donsetch::search::explain::run(&args[2..]).await;
            std::process::exit(code);
        }
        "fetch" | "search" | "crawl" | "screenshot" => {
            let code = cli::tool::run(cmd, &args[2..]).await;
            std::process::exit(code as i32);
        }

        // ── Developer: the search gold-set bench (live web, paced) ──
        "search-bench" => {
            let code = donsetch::search::bench::run(&args[2..]).await;
            std::process::exit(code);
        }

        // ── Discovery ──
        "tools" => {
            cli::args::Spec::new("tools", "donsetch tools [--json]", &["--json"])
                .reject(&args[2..]);
            cli::tool::print_tools_json()
        }

        // ── Management ──
        "config" => {
            cli::args::Spec::new(
                "config",
                "donsetch config show [--markdown] [--legacy]",
                &["--markdown", "--legacy", "show"],
            )
            .reject(&args[2..]);
            config_cli(&args);
        }
        "mcp" => {
            cli::args::Spec::new(
                "mcp",
                "donsetch mcp [--http] [--host HOST] [--port PORT] [--supervised]",
                &["--http", "--supervised"],
            )
            .with_valued(&["--host", "--port"])
            .reject(&args[2..]);
            donsetch::diag!(
                "[mcp] donsetch {} · {}",
                env!("CARGO_PKG_VERSION"),
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|e| e.to_string())
            );
            // Transport selection: the --http flag wins, then the
            // layered config ([transport] kind via TOML or env) for
            // launchers that can't pass flags, then stdio. Host/port:
            // flags beat the config. The config is already installed
            // by the block above; installing twice made install()
            // fail its one-shot check and the daemon exit.
            #[cfg(feature = "http")]
            if config::cfg().transport.kind == config::TransportKind::Http {
                let host = config::cfg().transport.host.clone();
                let port = config::cfg().transport.port;
                donsetch::diag!("[mcp] starting HTTP server on {}:{}", host, port);
                if let Err(e) = mcp::http::run(host, port).await {
                    donsetch::diag!("mcp HTTP server: {e}");
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
                donsetch::diag!(
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
                donsetch::diag!("[supervisor] donsetch mcp --supervised");
                if let Err(e) = mcp::supervisor::run() {
                    donsetch::diag!("[supervisor] {e}");
                    std::process::exit(1);
                }
            } else if let Err(e) = mcp::server::run().await {
                donsetch::diag!("mcp daemon: {e}");
                std::process::exit(1);
            }
        }
        "keys" => cli::keys::run(&args).await,
        "rules" => cli::rules::run(&args),

        "proxy" => cli::proxy::run(&args).await,
        "status" => cli::status::run(&args).await,
        "adapters" => match args.get(2).map(String::as_str) {
            Some("--help" | "-h") => cli::adapters::help(),
            _ => cli::adapters::run(&args),
        },
        "stop" => cli::stop::run(&args),
        "doctor" | "--doctor" => cli::doctor::run(&args).await,
        "update" | "-u" | "--update" => cli::update::run(&args).await,
        "rollback" | "--rollback" => cli::rollback::run(&args),
        "version" | "-v" | "-V" | "--version" => cli::version::run(&args).await,

        // ── Dev/internal (hidden from --help) ──
        "dev" => dev::dispatch(&args[2..]).await,
        // Backward-compat: bare dev commands still work.
        "probe" | "fingerprint" | "resume-test" | "extract" => {
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
        _ => cli::args::unknown_command(cmd, KNOWN_COMMANDS),
    }
}

// ── Dev commands ─────────────────────────────────────────

/// Route `donsetch help <command>` to the command's own help.
/// Falls back to top-level help for unknown commands.
async fn route_help(cmd: &str) {
    match cmd {
        "fetch" | "search" | "crawl" | "screenshot" => {
            // Re-invoke with --help (clap handles the output).
            let help_args = vec!["--help".to_string()];
            let _ = cli::tool::run(cmd, &help_args).await;
        }
        "keys" => cli::keys::help(),
        "adapters" => cli::adapters::help(),
        "rules" => cli::rules::help(),
        "status" => cli::status::help(),
        // `proxy`, `keys` and `adapters` carry their own text; routing here
        // means `donsetch help proxy` and `donsetch proxy --help` cannot
        // drift apart again (the copy that used to live here had gone
        // stale: it knew nothing of `proxy fetch|crawl on|off`).
        "proxy" => cli::proxy::help(),
        "--help" | "-h" => cli::tool::print_top_help(),
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
            println!("Usage: donsetch doctor [--deep] [--fix] [--json] [--improve] [--mcp]");
            println!();
            println!("  Health checks: binary, network, TLS, browser, browser launch,");
            println!("  auth profile, cache, permissions, PDFium, OCR models,");
            println!("  egress lanes, local rules.");
            println!("  Auto-fixes what it can. Reports issues with instructions.");
            println!("  --deep     live egress-lane and provider probes");
            println!("  --improve  explain the local self-improvement loop + receipts");
            println!("  --fix      apply safe mechanical repairs");
            println!("  --json     structured output for agents/CI");
        }
        "update" => {
            println!("Usage: donsetch update [--yes]");
            println!();
            println!("  Checks for a new release and downloads the platform-correct binary");
            println!("  from GitHub Releases. Verifies SHA256, replaces in place, saves backup.");
            println!("  A major version prints what it changes and asks first; --yes answers");
            println!("  for an unattended run.");
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
        "tools" => {
            println!("Usage: donsetch tools [--json]");
            println!();
            println!("  Prints the tool schemas as JSON (same as MCP tools/list).");
            println!("  --json is accepted and redundant: this output is always JSON.");
        }
        "stop" => {
            println!("Usage: donsetch stop");
            println!();
            println!("  Kills orphaned browser instances and cleans stale locks and");
            println!("  temp profiles. Run after a hard interrupt; harmless when there is");
            println!("  nothing to clean.");
        }
        _ => cli::args::unknown_command(cmd, KNOWN_COMMANDS),
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

#[cfg(test)]
mod upgrade_notice_tests {
    use super::has_v4_state;

    #[test]
    fn v4_state_is_recognised_by_its_vault_fields() {
        assert!(has_v4_state(br#"{"vault_epoch":3,"profiles":{}}"#));
        assert!(has_v4_state(
            br#"{"profiles":{"x":{"session_cookies":[]}}}"#
        ));
        assert!(has_v4_state(br#"{"tier1_cookies":[]}"#));
        assert!(!has_v4_state(
            br#"{"version":5,"profiles":{},"personas":{}}"#
        ));
        assert!(!has_v4_state(b""));
    }
}
